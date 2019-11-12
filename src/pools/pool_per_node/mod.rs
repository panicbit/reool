//! A connection pool for connecting to the nodes of a replica set
use std::sync::Arc;
use std::time::Instant;

use futures::future::Future;
use log::info;

use crate::config::Config;
use crate::connection_factory::ConnectionFactory;
use crate::error::InitializationResult;
use crate::executor_flavour::ExecutorFlavour;
use crate::instrumentation::InstrumentationFlavour;
use crate::pooled_connection::ConnectionFlavour;
use crate::{Checkout, CheckoutMode, Ping};

mod inner;

use self::inner::*;

/// A connection pool that maintains multiple connection pools
/// to a multiple Redis nodes whereas each inner pool is
/// connected to one Redis node
///
/// All the instances should be part of the same replica set.
/// You should only perform read operations on the
/// connections received from this kind of pool.
///
/// The replicas are selected in a round robin fashion.
///
/// The pool is cloneable and all clones share their connections.
/// Once the last instance drops the shared connections will be dropped.
pub(crate) struct PoolPerNode {
    inner: Arc<Inner>,
}

impl PoolPerNode {
    pub fn new<F, CF>(
        config: Config,
        create_connection_factory: F,
        executor_flavour: ExecutorFlavour,
        instrumentation: InstrumentationFlavour,
    ) -> InitializationResult<PoolPerNode>
    where
        CF: ConnectionFactory<Connection = ConnectionFlavour> + Send + Sync + 'static,
        F: Fn(Vec<String>) -> InitializationResult<CF>,
    {
        info!(
            "Creating pool per node for {:?} nodes",
            config.connect_to_nodes
        );

        let inner = Inner::new(
            config,
            create_connection_factory,
            executor_flavour,
            instrumentation,
        )?;

        Ok(PoolPerNode {
            inner: Arc::new(inner),
        })
    }

    pub fn check_out<M: Into<CheckoutMode>>(&self, mode: M) -> Checkout {
        self.inner.check_out(mode.into())
    }

    pub fn ping(&self, timeout: Instant) -> impl Future<Item = Vec<Ping>, Error = ()> + Send {
        self.inner.ping(timeout)
    }

    pub fn connected_to(&self) -> &[String] {
        &self.inner.connected_to
    }
}

impl Clone for PoolPerNode {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
