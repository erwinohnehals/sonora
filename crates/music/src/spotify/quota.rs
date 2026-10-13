use std::future::Future;
use std::time::{Duration, Instant};

use librespot_core::Error;
use librespot_core::error::ErrorKind;

/// How many requests a bulk lookup keeps in flight. librespot hands out ten requests a second
/// after a burst of 300, so a wide fan-out only queues against the quota and starves the
/// requests a listener is waiting on.
pub(crate) const FAN_OUT: usize = 8;

/// How long a call keeps waiting for room before it fails as it would have without waiting.
const PATIENCE: Duration = Duration::from_secs(90);
const PAUSE_MS: u64 = 500;
const JITTER_MS: u64 = 1500;

/// Runs a Spotify request, waiting out a spent request quota instead of failing. librespot
/// refuses any request past its quota without sending it, and Spotify answers a burst with 429;
/// both arrive as `ResourceExhausted` and clear within seconds. `call` builds the request
/// afresh for every try.
pub(crate) async fn patient<T, F, Fut>(mut call: F) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    let deadline = Instant::now() + PATIENCE;
    loop {
        match call().await {
            Err(error) if error.kind == ErrorKind::ResourceExhausted => {
                if Instant::now() >= deadline {
                    return Err(error);
                }
                tokio::time::sleep(pause()).await;
            }
            done => return done,
        }
    }
}

/// A pause with jitter, so the requests that hit the quota together do not all come back in
/// the same instant and spend the next second's room on one of them.
fn pause() -> Duration {
    Duration::from_millis(PAUSE_MS + fastrand::u64(0..=JITTER_MS))
}
