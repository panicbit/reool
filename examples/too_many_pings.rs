use std::env;
use std::time::{Duration, Instant};

use futures::future::{join_all, TryFutureExt};
use log::{debug, error, info};
use pretty_env_logger;
use tokio::runtime::Runtime;

use reool::{Commands, RedisPool};

/// Do many ping commands where many will faile because either
/// the checkout ties out or the checkout queue is full
fn main() {
    env::set_var("RUST_LOG", "reool=debug,too_many_pings=debug");
    let _ = pretty_env_logger::try_init();

    let runtime = Runtime::new().unwrap();

    let pool = RedisPool::builder()
        .connect_to_node("redis://127.0.0.1:6379")
        .desired_pool_size(10)
        .reservation_limit(Some(500))
        .checkout_timeout(Some(Duration::from_millis(150)))
        .task_executor(runtime.executor())
        .finish_redis_rs()
        .unwrap();

    info!("Do 1000 pings concurrently");
    let futs = (0..1_000)
        .map(|i| {
            let pool = &pool;

            async move {
                let result = pool
                    .check_out()
                    .err_into()
                    .and_then(|mut conn| async move {
                        conn.ping().await
                    });

                match result.await {
                    Err(err) => error!("PING {} failed: {}", i, err),
                    Ok(_) => debug!("PING {} OK", i),
                }
            }
        });

    let start = Instant::now();

    runtime.block_on(async move {
        join_all(futs).await;
        info!("finished pinging")
    });
    info!("PINGED 1000 times concurrently in {:?}", start.elapsed());

    drop(pool);
    info!("pool dropped");
    runtime.shutdown_on_idle();
}
