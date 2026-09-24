use std::time::Duration;

pub const IDLE_BEFORE_INDEX: Duration = Duration::from_millis(800);
pub const MODEL_IDLE_RELEASE: Duration = Duration::from_secs(60);
pub const INDEX_BATCH: usize = 4;

pub fn may_index(idle: Duration, paused: bool, query_pending: bool) -> bool {
    idle >= IDLE_BEFORE_INDEX && !paused && !query_pending
}

pub fn rest_after_batch(work: Duration) -> Duration {
    work.saturating_mul(3).max(Duration::from_millis(20))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexing_yields_to_typing_pause_and_queries() {
        assert!(!may_index(Duration::from_millis(799), false, false));
        assert!(!may_index(Duration::from_secs(2), true, false));
        assert!(!may_index(Duration::from_secs(2), false, true));
        assert!(may_index(IDLE_BEFORE_INDEX, false, false));
    }

    #[test]
    fn background_batches_have_a_limited_duty_cycle() {
        assert_eq!(rest_after_batch(Duration::ZERO), Duration::from_millis(20));
        assert_eq!(rest_after_batch(Duration::from_millis(100)), Duration::from_millis(300));
        assert_eq!(rest_after_batch(Duration::from_secs(1)), Duration::from_secs(3));
        assert_eq!(INDEX_BATCH, 4);
        assert_eq!(MODEL_IDLE_RELEASE, Duration::from_secs(60));
    }
}
