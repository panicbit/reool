//! # Reool
//!
//! Currently in early development.
//!
//! ## About
//!
//! Reool is a connection pool for Redis based on [redis-rs](https://crates.io/crates/redis).
//!
//! Currently `reool` is a fixed size connection pool.
//! `Reool` provides an interface for instrumentation.
//!
//!
//! You should also consider multiplexing instead of a pool based on your needs.
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

use futures::{future::Future, try_ready, Async, Poll};
use redis::{r#async::Connection, Client, RedisError};

use crate::connection_factory::{ConnectionFactory, NewConnection, NewConnectionError};
use crate::error::ReoolError;
use crate::pool::{CheckoutManaged, Poolable};

mod backoff_strategy;
mod commands;
pub mod connection_factory;
mod error;
pub(crate) mod executor_flavour;
pub(crate) mod helpers;
pub mod instrumentation;
pub mod multi_node_pool;
pub mod node_pool;
mod pool;
mod pooled_connection;

pub use commands::*;
pub use pooled_connection::PooledConnection;

/// A `Future` that represents a checkout.
///
/// A `Checkout` can fail for various reasons.
///
/// The most common ones are:
/// * There was a timeout on the checkout and it timed out
/// * The queue size was limited and the limit was reached
/// * There are simply no connections available
pub struct Checkout<T: Poolable>(CheckoutManaged<T>);

impl<T: Poolable> Future for Checkout<T> {
    type Item = PooledConnection<T>;
    type Error = ReoolError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let managed = try_ready!(self.0.poll());
        Ok(Async::Ready(PooledConnection {
            managed,
            last_op_completed: true,
        }))
    }
}

/// A trait that can be used as an interface for a connection pool.
pub trait RedisPool {
    type Connection: Poolable;
    /// Checkout a new connection and if the request has to be enqueued
    /// use a timeout as defined by the implementor.
    fn check_out(&self) -> Checkout<Self::Connection>;
    /// Checkout a new connection and if the request has to be enqueued
    /// use the given timeout or wait indefinetly.
    fn check_out_explicit_timeout(&self, timeout: Option<Duration>) -> Checkout<Self::Connection>;
}

impl Poolable for Connection {
    type Error = RedisError;
}

impl ConnectionFactory for Client {
    type Connection = Connection;

    fn create_connection(&self) -> NewConnection<Self::Connection> {
        NewConnection::new(self.get_async_connection().map_err(NewConnectionError::new))
    }
}
