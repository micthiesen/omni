//! Background services: started once after boot on the app tracker; a service
//! with a [`RetryPolicy`] is started again when its future ends before
//! shutdown, after a jittered exponential delay
//! (`min(exponential(base), max)`, jittered by 0.8 to 1.2).

use std::time::Duration;

use omni_runtime::{AppContext, BackgroundService, RetryPolicy};

const LOG: &str = "Main";
/// How long a service may take to wind down after shutdown begins.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

/// The delay before restart `attempt` (0-based), before jitter.
pub fn backoff_delay(policy: RetryPolicy, attempt: u32) -> Duration {
    let factor = 2u32.checked_pow(attempt).unwrap_or(u32::MAX);
    policy
        .initial
        .checked_mul(factor)
        .unwrap_or(policy.max)
        .min(policy.max)
}

/// Scales `delay` by a uniform factor in `[0.8, 1.2)`.
pub fn jittered(delay: Duration) -> Duration {
    delay.mul_f64(rand::random_range(0.8..1.2))
}

/// Runs `service` until shutdown, restarting it per its retry policy.
pub async fn run_service(ctx: AppContext, service: BackgroundService) {
    let mut attempt = 0u32;
    loop {
        let run = (service.start)(ctx.clone());
        tokio::pin!(run);
        tokio::select! {
            () = &mut run => {}
            () = ctx.shutdown.cancelled() => {
                // Services observe shutdown themselves (the dispatcher stops
                // its transport); one that does not is dropped after a grace.
                if tokio::time::timeout(SHUTDOWN_GRACE, &mut run).await.is_err() {
                    tracing::warn!(target: LOG, "{} did not stop within {} s", service.name, SHUTDOWN_GRACE.as_secs());
                }
                return;
            }
        }
        if ctx.shutdown.is_cancelled() {
            return;
        }
        let Some(policy) = service.retry else {
            return;
        };
        let delay = jittered(backoff_delay(policy, attempt));
        attempt = attempt.saturating_add(1);
        tracing::warn!(
            target: LOG,
            "{} stopped; restarting in {} ms",
            service.name,
            delay.as_millis()
        );
        tokio::select! {
            () = ctx.shutdown.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// Starts every service on the app tracker.
pub fn start_all(ctx: &AppContext, services: Vec<BackgroundService>) {
    for service in services {
        let name = service.name;
        tracing::debug!(target: LOG, "Starting service {name}");
        omni_core::spawn::spawn_tracked(
            &ctx.tracker,
            "background-service",
            run_service(ctx.clone(), service),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_30s_to_a_300s_cap() {
        let policy = RetryPolicy {
            initial: Duration::from_secs(30),
            max: Duration::from_secs(300),
        };
        let delays: Vec<u64> = (0..6).map(|a| backoff_delay(policy, a).as_secs()).collect();
        assert_eq!(delays, vec![30, 60, 120, 240, 300, 300]);
        assert_eq!(backoff_delay(policy, 40).as_secs(), 300);
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        for _ in 0..100 {
            let d = jittered(Duration::from_secs(100)).as_secs_f64();
            assert!((80.0..120.0).contains(&d), "{d}");
        }
    }
}
