//! Pure time helpers (SystemTime wrappers). No Inner, no locks, no statics.
//! Extracted from backend.rs (refactor phase 1, step 1).

pub(crate) fn unix_secs_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn epoch_millis_now_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn epoch_millis_now() -> String {
    epoch_millis_now_u64().to_string()
}

pub(crate) fn normalize_epoch_millis(timestamp: u64) -> u64 {
    if timestamp > 0 && timestamp < 1_000_000_000_000 {
        timestamp.saturating_mul(1000)
    } else {
        timestamp
    }
}

pub(crate) fn unix_nanos_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
