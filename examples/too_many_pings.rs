use std::env;
use std::time::{Duration, Instant};

use futures::future::{join_all, Future};
use log::{debug, error, info};
use pretty_env_logger;
use tokio::runtime::Runtime;

use reool::node_pool::SingleNodePool;
use reool::{Commands, RedisPool};

/// Do many ping commands where many will faile because either
/// the checkout ties out or the chackout queue is full
fn main() {
    env::set_var("RUST_LOG", "reool=debug,too_many_pings=debug");
    let _ = pretty_env_logger::try_init();

    let mut runtime = Runtime::new().unwrap();

    let pool = SingleNodePool::builder()
        .connect_to("redis://127.0.0.1:6379")
        .desired_pool_size(10)
        .reservation_limit(Some(500))
        .checkout_timeout(Some(Duration::from_millis(150)))
        .task_executor(runtime.executor())
        .finish()
        .unwrap();

    info!("Do 1000 pings concurrently");
    let futs: Vec<_> = (0..1_000)
        .map(|i| {
            pool.check_out()
                .from_err()
                .and_then(Commands::ping)
                .then(move |res| match res {
                    Err(err) => {
                        error!("PING {} failed: {}", i, err);
                        Ok(())
                    }
                    Ok(_) => {
                        debug!("PING {} OK", i);
                        Ok::<_, ()>(())
                    }
                })
        })
        .collect();

    let fut = join_all(futs).map(|_| {
        info!("finished pinging");
    });

    let start = Instant::now();
    runtime.block_on(fut).unwrap();
    info!("PINGED 1000 times concurrently in {:?}", start.elapsed());

    drop(pool);
    info!("pool dropped");
    runtime.shutdown_on_idle().wait().unwrap();
}