use crate::connection_factory::ConnectionFactory;
use crate::connection_factory::NewConnectionError;
use std::error::Error as StdError;
use std::fmt;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures::{
    future::{self, Future, Loop},
    stream::Stream,
    sync::mpsc,
    Poll,
};
use log::{debug, trace, warn};
use tokio_timer::Delay;

use crate::backoff_strategy::BackoffStrategy;
use crate::error::CheckoutError;
use crate::executor_flavour::*;
use crate::instrumentation::Instrumentation;
use crate::Poolable;

use inner_pool::{CheckInParcel, InnerPool};

mod inner_pool;

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub desired_pool_size: usize,
    pub backoff_strategy: BackoffStrategy,
    pub reservation_limit: Option<usize>,
    pub stats_interval: Duration,
}

#[cfg(test)]
impl Config {
    pub fn desired_pool_size(mut self, v: usize) -> Self {
        self.desired_pool_size = v;
        self
    }

    pub fn backoff_strategy(mut self, v: BackoffStrategy) -> Self {
        self.backoff_strategy = v;
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            desired_pool_size: 20,
            backoff_strategy: BackoffStrategy::default(),
            reservation_limit: Some(50),
            stats_interval: Duration::from_millis(100),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MinMax<T = usize>(pub T, pub T);

impl<T> MinMax<T>
where
    T: Copy,
{
    pub fn min(&self) -> T {
        self.0
    }
    pub fn max(&self) -> T {
        self.1
    }
}

impl<T> Default for MinMax<T>
where
    T: Default,
{
    fn default() -> Self {
        Self(T::default(), T::default())
    }
}

/// Simple statistics on the internals of the pool.
///
/// The values are not very accurate since they
/// are only the minimum and maximum values
/// observed during a configurable interval.
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// The amount of connections
    pub pool_size: MinMax,
    /// The number of connections that are currently checked out
    pub in_flight: MinMax,
    /// The number of pending requests for connections
    pub reservations: MinMax,
    /// The number of idle connections which are available for
    /// immediate checkout
    pub idle: MinMax,
    /// The number of accessible nodes.
    ///
    /// Unless connected to multiple nodes this value will be 1.
    pub node_count: usize,
}

impl Default for PoolStats {
    fn default() -> Self {
        Self {
            pool_size: MinMax::default(),
            in_flight: MinMax::default(),
            reservations: MinMax::default(),
            idle: MinMax::default(),
            node_count: 0,
        }
    }
}

pub(crate) struct Pool<T: Poolable> {
    inner_pool: Arc<InnerPool<T>>,
}

impl<T> Pool<T>
where
    T: Poolable,
{
    pub fn new<C, I>(
        config: Config,
        connection_factory: C,
        executor: ExecutorFlavour,
        instrumentation: Option<I>,
    ) -> Self
    where
        C: ConnectionFactory<Connection = T> + Send + Sync + 'static,
        I: Instrumentation + Send + Sync + 'static,
    {
        let (new_con_tx, new_conn_rx) = mpsc::unbounded();

        let num_connections = config.desired_pool_size;
        let inner_pool = Arc::new(InnerPool::new(
            config.clone(),
            new_con_tx.clone(),
            instrumentation,
        ));

        start_new_conn_stream(
            new_conn_rx,
            Arc::new(connection_factory),
            Arc::downgrade(&inner_pool),
            executor,
            config.backoff_strategy,
        );

        let pool = Self { inner_pool };

        (0..num_connections).for_each(|_| {
            pool.add_new_connection();
        });

        pool
    }

    #[cfg(test)]
    pub fn no_instrumentation<C>(
        config: Config,
        connection_factory: C,
        executor: ExecutorFlavour,
    ) -> Self
    where
        C: ConnectionFactory<Connection = T> + Send + Sync + 'static,
    {
        Self::new::<_, ()>(config, connection_factory, executor, None)
    }

    pub fn check_out(&self, timeout: Option<Duration>) -> CheckoutManaged<T> {
        self.inner_pool.check_out(timeout)
    }

    pub fn add_new_connection(&self) {
        trace!("add new connection request");
        self.inner_pool.request_new_conn();
    }

    pub fn remove_connection(&self) {
        self.inner_pool.remove_conn()
    }

    pub fn stats(&self) -> PoolStats {
        self.inner_pool.stats()
    }

    pub fn trigger_stats(&self) {
        self.inner_pool.trigger_stats()
    }

    #[cfg(test)]
    #[allow(unused)]
    fn inner_pool(&self) -> &Arc<InnerPool<T>> {
        &self.inner_pool
    }
}

fn start_new_conn_stream<T, C>(
    receiver: mpsc::UnboundedReceiver<NewConnMessage>,
    connection_factory: Arc<C>,
    inner_pool: Weak<InnerPool<T>>,
    executor: ExecutorFlavour,
    back_off_strategy: BackoffStrategy,
) where
    T: Poolable,
    C: ConnectionFactory<Connection = T> + Send + Sync + 'static,
{
    let spawn_handle = executor.spawn_unbounded(receiver);

    let mut is_shut_down = false;
    let fut = spawn_handle.for_each(move |msg| {
        if is_shut_down {
            trace!("new connection requested on finished stream");
            Box::new(future::err(()))
        } else {
            match msg {
                NewConnMessage::RequestNewConn => {
                    if let Some(existing_inner_pool) = inner_pool.upgrade() {
                        trace!("creating new connection");
                        let fut = create_new_managed(
                            Instant::now(),
                            connection_factory.clone(),
                            Arc::downgrade(&existing_inner_pool),
                            back_off_strategy,
                        )
                        .map(|_| ())
                        .map_err(|err| warn!("Failed to create new connection: {}", err));
                        drop(existing_inner_pool);
                        Box::new(fut)
                    } else {
                        warn!("attempt to create new connection even though the pool is gone");
                        Box::new(future::err(())) as Box<Future<Item = _, Error = _> + Send>
                    }
                }
                NewConnMessage::Shutdown => {
                    debug!("shutdown new conn stream");
                    is_shut_down = true;
                    Box::new(future::err(()))
                }
            }
        }
    });

    executor.execute(fut).unwrap()
}

impl<T: Poolable> Clone for Pool<T> {
    fn clone(&self) -> Self {
        Self {
            inner_pool: self.inner_pool.clone(),
        }
    }
}

fn create_new_managed<T: Poolable, C>(
    initiated_at: Instant,
    connection_factory: Arc<C>,
    weak_inner_pool: Weak<InnerPool<T>>,
    back_off_strategy: BackoffStrategy,
) -> NewManaged<T>
where
    T: Poolable,
    C: ConnectionFactory<Connection = T> + Send + Sync + 'static,
{
    let fut = future::loop_fn((weak_inner_pool, 1), move |(weak_inner, attempt)| {
        if let Some(inner_pool) = weak_inner.upgrade() {
            let start_connect = Instant::now();
            let fut = connection_factory
                .create_connection()
                .then(move |res| match res {
                    Ok(conn) => {
                        trace!("new connection created");
                        inner_pool.notify_connection_created(
                            initiated_at.elapsed(),
                            start_connect.elapsed(),
                        );
                        Box::new(future::ok(Loop::Break(Managed::fresh(
                            conn,
                            Arc::downgrade(&inner_pool),
                        ))))
                    }
                    Err(err) => {
                        inner_pool.notify_connection_factory_failed();
                        if let Some(backoff) = back_off_strategy.get_next_backoff(attempt) {
                            let delay = Delay::new(Instant::now() + backoff);
                            warn!(
                            "Attempt {} to create a connection failed. Retry in {:?}. Error: {}",
                            attempt, backoff, err
                        );
                            Box::new(
                                delay
                                    .map_err(|err| NewConnectionError::new(Box::new(err)))
                                    .and_then(move |()| {
                                        future::ok(Loop::Continue((
                                            Arc::downgrade(&inner_pool),
                                            attempt + 1,
                                        )))
                                    }),
                            )
                                as Box<dyn Future<Item = _, Error = _> + Send>
                        } else {
                            warn!(
                        "Attempt {} to create a connection failed. Retry immediately. Error: {}",
                        attempt, err);
                            Box::new(future::ok(Loop::Continue((
                                Arc::downgrade(&inner_pool),
                                attempt + 1,
                            ))))
                        }
                    }
                });
            Box::new(fut) as Box<dyn Future<Item = _, Error = _> + Send>
        } else {
            Box::new(future::err(NewConnectionError::new(Box::new(
                PoolIsGoneError,
            ))))
        }
    });
    NewManaged::new(fut)
}

pub(crate) struct CheckoutManaged<T: Poolable> {
    inner: Box<Future<Item = Managed<T>, Error = CheckoutError> + Send + 'static>,
}

impl<T: Poolable> CheckoutManaged<T> {
    pub fn new<F>(fut: F) -> Self
    where
        F: Future<Item = Managed<T>, Error = CheckoutError> + Send + 'static,
    {
        Self {
            inner: Box::new(fut),
        }
    }
}

impl<T: Poolable> Future for CheckoutManaged<T> {
    type Item = Managed<T>;
    type Error = CheckoutError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll()
    }
}

pub(crate) struct Managed<T: Poolable> {
    created_at: Instant,
    checked_out_at: Option<Instant>,
    pub value: Option<T>,
    inner_pool: Weak<InnerPool<T>>,
    marked_for_kill: bool,
}

impl<T: Poolable> Managed<T> {
    pub fn fresh(value: T, inner_pool: Weak<InnerPool<T>>) -> Self {
        Managed {
            value: Some(value),
            inner_pool,
            marked_for_kill: false,
            created_at: Instant::now(),
            checked_out_at: None,
        }
    }
}

impl<T: Poolable> Drop for Managed<T> {
    fn drop(&mut self) {
        if let Some(inner_pool) = self.inner_pool.upgrade() {
            if self.marked_for_kill {
                inner_pool.check_in(CheckInParcel::Killed(self.created_at.elapsed()))
            } else if let Some(value) = self.value.take() {
                inner_pool.check_in(CheckInParcel::Alive(Managed {
                    inner_pool: Arc::downgrade(&inner_pool),
                    value: Some(value),
                    marked_for_kill: false,
                    created_at: self.created_at,
                    checked_out_at: self.checked_out_at,
                }));
            } else {
                debug!("no value - drop connection and request new one");
                // No connection. Create a new one.
                inner_pool.check_in(CheckInParcel::Dropped(
                    self.checked_out_at.as_ref().map(Instant::elapsed),
                    self.created_at.elapsed(),
                ));
                inner_pool.request_new_conn();
            }
        } else {
            trace!("terminating connection because the pool is gone")
        }
    }
}

pub(crate) enum NewConnMessage {
    RequestNewConn,
    Shutdown,
}

pub(crate) struct NewManaged<T: Poolable> {
    inner: Box<Future<Item = Managed<T>, Error = NewConnectionError> + Send + 'static>,
}

impl<T: Poolable> NewManaged<T> {
    pub fn new<F>(f: F) -> Self
    where
        F: Future<Item = Managed<T>, Error = NewConnectionError> + Send + 'static,
    {
        Self { inner: Box::new(f) }
    }
}

impl<T: Poolable> Future for NewManaged<T> {
    type Item = Managed<T>;
    type Error = NewConnectionError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll()
    }
}

#[derive(Debug)]
struct PoolIsGoneError;

impl fmt::Display for PoolIsGoneError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.description())
    }
}

impl StdError for PoolIsGoneError {
    fn description(&self) -> &str {
        "the pool was already gone"
    }

    fn cause(&self) -> Option<&StdError> {
        None
    }
}

#[cfg(test)]
mod test;
