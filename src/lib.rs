//! # Reool
//!
//! ## About
//!
//! Reool is a REdis connection pOOL based on [redis-rs](https://crates.io/crates/redis).
//!
//! Reool is aimed at either connecting to a single primary node or
//! connecting to a replica set using the replicas as read only nodes.
//!
//! Currently Reool is a fixed size connection pool.
//! Reool provides an interface for instrumentation.
//!
//! You should also consider multiplexing instead of a pool based upon your needs.
//!
//! The `PooledConnection` of `reool` implements the `ConnectionLike`
//! interface of [redis-rs](https://crates.io/crates/redis) for easier integration.
//!
//! For documentation visit [crates.io](https://crates.io/crates/reool).
//!
//! ## License
//!
//! Reool is distributed under the terms of both the MIT license and the
//! Apache License (Version 2.0).
//!
//! See LICENSE-APACHE and LICENSE-MIT for details.
//! License: Apache-2.0/MIT
use std::time::Duration;

use futures::{
    future::{self, Future},
    try_ready, Async, Poll,
};

use crate::config::Builder;
use crate::pooled_connection::ConnectionFlavour;
use crate::pools::pool_internal::{CheckoutManaged, Managed};

pub mod config;
pub mod instrumentation;

pub use crate::error::{CheckoutError, CheckoutErrorKind};
pub use commands::Commands;
pub use pooled_connection::RedisConnection;

pub(crate) mod connection_factory;
pub(crate) mod executor_flavour;
pub(crate) mod helpers;

mod activation_order;
mod backoff_strategy;
mod commands;
mod error;
mod pooled_connection;
mod pools;
mod redis_rs;

pub trait Poolable: Send + Sized + 'static {
    fn connected_to(&self) -> &str;
}

/// A `Future` that represents a checkout.
///
/// A `Checkout` can fail for various reasons.
///
/// The most common ones are:
/// * There was a timeout on the checkout and it timed out
/// * The queue size was limited and the limit was reached
/// * There are simply no connections available
/// * There is no connected node
pub struct Checkout(CheckoutManaged<ConnectionFlavour>);

impl Checkout {
    pub(crate) fn new<F>(f: F) -> Self
    where
        F: Future<Item = Managed<ConnectionFlavour>, Error = CheckoutError> + Send + 'static,
    {
        Checkout(CheckoutManaged::new(f))
    }
}

impl Future for Checkout {
    type Item = RedisConnection;
    type Error = CheckoutError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let managed = try_ready!(self.0.poll());
        Ok(Async::Ready(RedisConnection {
            managed,
            connection_state_ok: true,
        }))
    }
}

#[derive(Clone)]
enum RedisPoolFlavour {
    Empty,
    Shared(pools::SharedPool),
    PerNode(pools::PoolPerNode),
}

/// A pool to one or more Redis instances.
#[derive(Clone)]
pub struct RedisPool(RedisPoolFlavour);

impl RedisPool {
    pub fn builder() -> Builder {
        Builder::default()
    }

    pub fn no_pool() -> Self {
        RedisPool(RedisPoolFlavour::Empty)
    }

    /// Checkout a new connection and if the request has to be enqueued
    /// use a timeout as defined by the pool as a default.
    pub fn check_out(&self) -> Checkout {
        match self.0 {
            RedisPoolFlavour::Shared(ref pool) => pool.check_out(),
            RedisPoolFlavour::PerNode(ref pool) => pool.check_out(),
            RedisPoolFlavour::Empty => Checkout(CheckoutManaged::new(future::err(
                CheckoutError::new(CheckoutErrorKind::NoPool),
            ))),
        }
    }
    /// Checkout a new connection and if the request has to be enqueued
    /// use the given timeout or wait indefinitely if `timeout` is `None`.
    pub fn check_out_explicit_timeout(&self, timeout: Option<Duration>) -> Checkout {
        match self.0 {
            RedisPoolFlavour::Shared(ref pool) => pool.check_out_explicit_timeout(timeout),
            RedisPoolFlavour::PerNode(ref pool) => pool.check_out_explicit_timeout(timeout),
            RedisPoolFlavour::Empty => Checkout(CheckoutManaged::new(future::err(
                CheckoutError::new(CheckoutErrorKind::NoPool),
            ))),
        }
    }

    /// Ping all the nodes which this pool is connected to.
    ///
    /// `timeout` is the maximum time allowed for a ping.
    pub fn ping(&self, timeout: Duration) -> impl Future<Item = Vec<Ping>, Error = ()> + Send {
        match self.0 {
            RedisPoolFlavour::Shared(ref pool) => Box::new(pool.ping(timeout).map(|p| vec![p]))
                as Box<dyn Future<Item = _, Error = ()> + Send>,
            RedisPoolFlavour::PerNode(ref pool) => Box::new(pool.ping(timeout)),
            RedisPoolFlavour::Empty => Box::new(future::ok(vec![])),
        }
    }

    pub fn connected_to(&self) -> &[String] {
        match self.0 {
            RedisPoolFlavour::Shared(ref pool) => pool.connected_to(),
            RedisPoolFlavour::PerNode(ref pool) => pool.connected_to(),
            RedisPoolFlavour::Empty => &[],
        }
    }
}

#[derive(Debug)]
pub enum PingState {
    Ok,
    Failed(Box<dyn std::error::Error + Send>),
}

#[derive(Debug)]
pub struct Ping {
    pub latency: Duration,
    pub uri: Option<String>,
    pub state: PingState,
}

impl Ping {
    pub fn is_ok(&self) -> bool {
        match self.state {
            PingState::Ok => true,
            _ => false,
        }
    }

    pub fn is_failed(&self) -> bool {
        !self.is_ok()
    }
}
