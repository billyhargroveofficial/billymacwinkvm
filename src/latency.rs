use std::sync::OnceLock;
use std::time::Duration;

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("SOFTKVM_LATENCY_LOG")
            .map(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on" | "info" | "trace"
                )
            })
            .unwrap_or(false)
    })
}

pub fn slow(elapsed: Duration) -> bool {
    elapsed >= warn_threshold()
}

pub fn report(_elapsed: Duration) -> bool {
    enabled()
}

pub fn ms(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

fn warn_threshold() -> Duration {
    static THRESHOLD: OnceLock<Duration> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        let ms = std::env::var("SOFTKVM_LATENCY_WARN_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(8);
        Duration::from_millis(ms)
    })
}
