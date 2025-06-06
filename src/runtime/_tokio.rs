use crate::{
  clients::WithOptions,
  commands,
  error::Error,
  interfaces::{default_send_command, FredResult},
  modules::inner::ClientInner,
  protocol::command::Command,
  router::commands as router_commands,
  types::{
    config::{Config, ConnectionConfig, Options, PerformanceConfig, ReconnectPolicy, Server},
    ClientState,
    ConnectHandle,
    CustomCommand,
    FromValue,
    InfoKind,
    Resp3Frame,
    RespVersion,
    Value,
    Version,
  },
  utils,
};
use arc_swap::ArcSwapAny;
use futures::{Stream, StreamExt};
use std::{future::Future, sync::Arc};
use tokio::sync::mpsc::{
  channel as bounded_channel,
  error::{TryRecvError, TrySendError},
  unbounded_channel,
  Receiver as BoundedReceiver,
  Sender as BoundedSender,
  UnboundedReceiver,
  UnboundedSender,
};
pub use tokio::{
  spawn,
  sync::{
    broadcast::{
      self,
      error::SendError as BroadcastSendError,
      Receiver as BroadcastReceiver,
      Sender as BroadcastSender,
    },
    oneshot::{channel as oneshot_channel, Receiver as OneshotReceiver, Sender as OneshotSender},
    RwLock as AsyncRwLock,
  },
  task::JoinHandle,
  time::sleep,
};
use tokio_stream::wrappers::{ReceiverStream, UnboundedReceiverStream};

#[cfg(feature = "dynamic-pool")]
use arc_swap::ArcSwapOption;

enum SenderKind<T: Send + 'static> {
  Bounded(BoundedSender<T>),
  Unbounded(UnboundedSender<T>),
}

impl<T: Send + 'static> Clone for SenderKind<T> {
  fn clone(&self) -> Self {
    match self {
      SenderKind::Bounded(tx) => SenderKind::Bounded(tx.clone()),
      SenderKind::Unbounded(tx) => SenderKind::Unbounded(tx.clone()),
    }
  }
}

pub struct Sender<T: Send + 'static> {
  tx: SenderKind<T>,
}

impl<T: Send + 'static> Clone for Sender<T> {
  fn clone(&self) -> Self {
    Sender { tx: self.tx.clone() }
  }
}

impl<T: Send + 'static> Sender<T> {
  pub async fn send(&self, val: T) -> Result<(), T> {
    match self.tx {
      SenderKind::Bounded(ref tx) => tx.send(val).await.map_err(|e| e.0),
      SenderKind::Unbounded(ref tx) => tx.send(val).map_err(|e| e.0),
    }
  }

  pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
    match self.tx {
      SenderKind::Bounded(ref tx) => tx.try_send(val),
      SenderKind::Unbounded(ref tx) => tx.send(val).map_err(|e| TrySendError::Closed(e.0)),
    }
  }
}

enum ReceiverKind<T: Send + 'static> {
  Bounded(BoundedReceiver<T>),
  Unbounded(UnboundedReceiver<T>),
}

pub struct Receiver<T: Send + 'static> {
  rx: ReceiverKind<T>,
}

impl<T: Send + 'static> Receiver<T> {
  pub async fn recv(&mut self) -> Option<T> {
    match self.rx {
      ReceiverKind::Bounded(ref mut tx) => tx.recv().await,
      ReceiverKind::Unbounded(ref mut tx) => tx.recv().await,
    }
  }

  pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
    match self.rx {
      ReceiverKind::Bounded(ref mut tx) => tx.try_recv(),
      ReceiverKind::Unbounded(ref mut tx) => tx.try_recv(),
    }
  }

  pub fn into_stream(self) -> impl Stream<Item = T> + 'static {
    match self.rx {
      ReceiverKind::Bounded(tx) => ReceiverStream::new(tx).boxed(),
      ReceiverKind::Unbounded(tx) => UnboundedReceiverStream::new(tx).boxed(),
    }
  }
}

pub fn channel<T: Send + 'static>(size: usize) -> (Sender<T>, Receiver<T>) {
  if size == 0 {
    let (tx, rx) = unbounded_channel();
    (
      Sender {
        tx: SenderKind::Unbounded(tx),
      },
      Receiver {
        rx: ReceiverKind::Unbounded(rx),
      },
    )
  } else {
    let (tx, rx) = bounded_channel(size);
    (
      Sender {
        tx: SenderKind::Bounded(tx),
      },
      Receiver {
        rx: ReceiverKind::Bounded(rx),
      },
    )
  }
}

#[cfg(any(feature = "dns", feature = "trust-dns-resolver"))]
use crate::protocol::types::Resolve;
#[cfg(feature = "i-server")]
use crate::types::ShutdownFlags;

/// The reference counting container type.
///
/// This type may change based on the runtime feature flags used.
pub type RefCount<T> = Arc<T>;

pub type AtomicBool = std::sync::atomic::AtomicBool;
pub type AtomicUsize = std::sync::atomic::AtomicUsize;
pub type Mutex<T> = parking_lot::Mutex<T>;
pub type RwLock<T> = parking_lot::RwLock<T>;
pub type RefSwap<T> = ArcSwapAny<T>;

#[cfg(feature = "dynamic-pool")]
pub type RefSwapOption<T> = ArcSwapOption<T>;

pub fn broadcast_send<T: Clone, F: Fn(&T)>(tx: &BroadcastSender<T>, msg: &T, func: F) {
  if let Err(BroadcastSendError(val)) = tx.send(msg.clone()) {
    func(&val);
  }
}

pub fn broadcast_channel<T: Clone>(capacity: usize) -> (BroadcastSender<T>, BroadcastReceiver<T>) {
  broadcast::channel(capacity)
}

/// Any client that implements any part of the server interface.
pub trait ClientLike: Clone + Send + Sync + Sized {
  #[doc(hidden)]
  fn inner(&self) -> &Arc<ClientInner>;

  /// Helper function to intercept and modify a command without affecting how it is sent to the connection layer.
  #[doc(hidden)]
  fn change_command(&self, _: &mut Command) {}

  /// Helper function to intercept and customize how a command is sent to the connection layer.
  #[doc(hidden)]
  fn send_command<C>(&self, command: C) -> Result<(), Error>
  where
    C: Into<Command>,
  {
    let mut command: Command = command.into();
    self.change_command(&mut command);
    default_send_command(self.inner(), command)
  }

  /// The unique ID identifying this client and underlying connections.
  fn id(&self) -> &str {
    &self.inner().id
  }

  /// Read the config used to initialize the client.
  fn client_config(&self) -> Config {
    self.inner().config.as_ref().clone()
  }

  /// Read the reconnect policy used to initialize the client.
  fn client_reconnect_policy(&self) -> Option<ReconnectPolicy> {
    self.inner().policy.read().clone()
  }

  /// Read the connection config used to initialize the client.
  fn connection_config(&self) -> &ConnectionConfig {
    self.inner().connection.as_ref()
  }

  /// Read the RESP version used by the client when communicating with the server.
  fn protocol_version(&self) -> RespVersion {
    if self.inner().is_resp3() {
      RespVersion::RESP3
    } else {
      RespVersion::RESP2
    }
  }

  /// Whether the client has a reconnection policy.
  fn has_reconnect_policy(&self) -> bool {
    self.inner().policy.read().is_some()
  }

  /// Whether the client is connected to a cluster.
  fn is_clustered(&self) -> bool {
    self.inner().config.server.is_clustered()
  }

  /// Whether the client uses the sentinel interface.
  fn uses_sentinels(&self) -> bool {
    self.inner().config.server.is_sentinel()
  }

  /// Update the internal [PerformanceConfig](crate::types::config::PerformanceConfig) in place with new values.
  fn update_perf_config(&self, config: PerformanceConfig) {
    self.inner().update_performance_config(config);
  }

  /// Read the [PerformanceConfig](crate::types::config::PerformanceConfig) associated with this client.
  fn perf_config(&self) -> PerformanceConfig {
    self.inner().performance_config()
  }

  /// Read the state of the underlying connection(s).
  ///
  /// If running against a cluster the underlying state will reflect the state of the least healthy connection.
  fn state(&self) -> ClientState {
    self.inner().state.read().clone()
  }

  /// Whether all underlying connections are healthy.
  fn is_connected(&self) -> bool {
    *self.inner().state.read() == ClientState::Connected
  }

  /// Read the set of active connections managed by the client.
  fn active_connections(&self) -> Vec<Server> {
    self.inner().active_connections()
  }

  /// Read the server version, if known.
  fn server_version(&self) -> Option<Version> {
    self.inner().server_state.read().kind.server_version()
  }

  /// Override the DNS resolution logic for the client.
  #[cfg(feature = "dns")]
  #[cfg_attr(docsrs, doc(cfg(feature = "dns")))]
  fn set_resolver(&self, resolver: Arc<dyn Resolve>) -> impl Future + Send {
    async move { self.inner().set_resolver(resolver).await }
  }

  /// Connect to the server.
  ///
  /// This function returns a `JoinHandle` to a task that drives the connection. It will not resolve until the
  /// connection closes, or if a reconnection policy with unlimited attempts is provided then it will
  /// run until `QUIT` is called. Callers should avoid calling [abort](tokio::task::JoinHandle::abort) on the returned
  /// `JoinHandle` unless the client will no longer be used.
  ///
  /// **Calling this function more than once will drop all state associated with the previous connection(s).** Any
  /// pending commands on the old connection(s) will either finish or timeout, but they will not be retried on the
  /// new connection(s).
  ///
  /// See [init](Self::init) for an alternative shorthand.
  fn connect(&self) -> ConnectHandle {
    let inner = self.inner().clone();
    utils::reset_router_task(&inner);

    tokio::spawn(async move {
      inner.backchannel.clear_router_state(&inner).await;
      let result = router_commands::start(&inner).await;
      // a canceled error means we intentionally closed the client
      _trace!(inner, "Ending connection task with {:?}", result);

      if let Err(ref error) = result {
        if !error.is_canceled() {
          inner.notifications.broadcast_connect(Err(error.clone()));
        }
      }

      inner.cas_client_state(ClientState::Disconnecting, ClientState::Disconnected);
      result
    })
  }

  /// Force a reconnection to the server(s).
  ///
  /// When running against a cluster this function will also refresh the cached cluster routing table.
  fn force_reconnection(&self) -> impl Future<Output = FredResult<()>> + Send {
    async move { commands::server::force_reconnection(self.inner()).await }
  }

  /// Wait for the result of the next connection attempt.
  ///
  /// This can be used with `on_reconnect` to separate initialization logic that needs to occur only on the next
  /// connection attempt vs all subsequent attempts.
  fn wait_for_connect(&self) -> impl Future<Output = FredResult<()>> + Send {
    async move {
      if utils::read_locked(&self.inner().state) == ClientState::Connected {
        debug!("{}: Client is already connected.", self.inner().id);
        Ok(())
      } else {
        self.inner().notifications.connect.load().subscribe().recv().await?
      }
    }
  }

  /// Initialize a new routing and connection task and wait for it to connect successfully.
  ///
  /// The returned [ConnectHandle](crate::types::ConnectHandle) refers to the task that drives the routing and
  /// connection layer. It will not finish until the max reconnection count is reached. Callers should avoid calling
  /// [abort](tokio::task::JoinHandle::abort) on the returned `JoinHandle` unless the client will no longer be used.
  ///
  /// Callers can also use [connect](Self::connect) and [wait_for_connect](Self::wait_for_connect) separately if
  /// needed.
  ///
  /// ```rust
  /// use fred::prelude::*;
  ///
  /// #[tokio::main]
  /// async fn main() -> Result<(), Error> {
  ///   let client = Client::default();
  ///   let connection_task = client.init().await?;
  ///
  ///   // ...
  ///
  ///   client.quit().await?;
  ///   connection_task.await?
  /// }
  /// ```
  fn init(&self) -> impl Future<Output = FredResult<ConnectHandle>> + Send {
    async move {
      let mut rx = { self.inner().notifications.connect.load().subscribe() };
      let task = self.connect();
      let error = rx.recv().await.map_err(Error::from).and_then(|r| r).err();

      if let Some(error) = error {
        // the initial connection failed, so we should gracefully close the routing task
        utils::reset_router_task(self.inner());
        Err(error)
      } else {
        Ok(task)
      }
    }
  }

  /// Close the connection to the server. The returned future resolves when the command has been written to the
  /// socket, not when the connection has been fully closed. Some time after this future resolves the future
  /// returned by [connect](Self::connect) will resolve which indicates that the connection has been fully closed.
  ///
  /// This function will wait for pending commands to finish, and will also close all error, pubsub message, and
  /// reconnection event streams.
  fn quit(&self) -> impl Future<Output = FredResult<()>> + Send {
    async move { commands::server::quit(self).await }
  }

  /// Shut down the server and quit the client.
  ///
  /// <https://redis.io/commands/shutdown>
  #[cfg(feature = "i-server")]
  #[cfg_attr(docsrs, doc(cfg(feature = "i-server")))]
  fn shutdown(&self, flags: Option<ShutdownFlags>) -> impl Future<Output = FredResult<()>> + Send {
    async move { commands::server::shutdown(self, flags).await }
  }

  /// Delete the keys in all databases.
  ///
  /// <https://redis.io/commands/flushall>
  fn flushall<R>(&self, r#async: bool) -> impl Future<Output = FredResult<R>> + Send
  where
    R: FromValue,
  {
    async move { commands::server::flushall(self, r#async).await?.convert() }
  }

  /// Delete the keys on all nodes in the cluster. This is a special function that does not map directly to the server
  /// interface.
  fn flushall_cluster(&self) -> impl Future<Output = FredResult<()>> + Send {
    async move { commands::server::flushall_cluster(self).await }
  }

  /// Ping the server.
  ///
  /// <https://redis.io/commands/ping>
  fn ping<R>(&self, message: Option<String>) -> impl Future<Output = FredResult<R>> + Send
  where
    R: FromValue,
  {
    async move { commands::server::ping(self, message).await?.convert() }
  }

  /// Read info about the server.
  ///
  /// <https://redis.io/commands/info>
  fn info<R>(&self, section: Option<InfoKind>) -> impl Future<Output = FredResult<R>> + Send
  where
    R: FromValue,
  {
    async move { commands::server::info(self, section).await?.convert() }
  }

  /// Run a custom command that is not yet supported via another interface on this client. This is most useful when
  /// interacting with third party modules or extensions.
  ///
  /// Callers should use the re-exported [redis_keyslot](crate::util::redis_keyslot) function to hash the command's
  /// key, if necessary.
  ///
  /// This interface should be used with caution as it may break the automatic pipeline features in the client if
  /// command flags are not properly configured.
  fn custom<R, T>(&self, cmd: CustomCommand, args: Vec<T>) -> impl Future<Output = FredResult<R>> + Send
  where
    R: FromValue,
    T: TryInto<Value> + Send,
    T::Error: Into<Error> + Send,
  {
    async move {
      let args = utils::try_into_vec(args)?;
      commands::server::custom(self, cmd, args).await?.convert()
    }
  }

  /// Run a custom command similar to [custom](Self::custom), but return the response frame directly without any
  /// parsing.
  ///
  /// Note: RESP2 frames from the server are automatically converted to the RESP3 format when parsed by the client.
  fn custom_raw<T>(&self, cmd: CustomCommand, args: Vec<T>) -> impl Future<Output = FredResult<Resp3Frame>> + Send
  where
    T: TryInto<Value> + Send,
    T::Error: Into<Error> + Send,
  {
    async move {
      let args = utils::try_into_vec(args)?;
      commands::server::custom_raw(self, cmd, args).await
    }
  }

  /// Customize various configuration options on commands.
  fn with_options(&self, options: &Options) -> WithOptions<Self> {
    WithOptions {
      client:  self.clone(),
      options: options.clone(),
    }
  }
}

pub fn spawn_event_listener<T, F, Fut>(mut rx: BroadcastReceiver<T>, func: F) -> JoinHandle<FredResult<()>>
where
  T: Clone + Send + 'static,
  Fut: Future<Output = FredResult<()>> + Send + 'static,
  F: Fn(T) -> Fut + Send + 'static,
{
  tokio::spawn(async move {
    let mut result = Ok(());

    while let Ok(val) = rx.recv().await {
      if let Err(err) = func(val).await {
        result = Err(err);
        break;
      }
    }

    result
  })
}
