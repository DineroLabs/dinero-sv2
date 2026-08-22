//! Pure stale-job rule shared by solo, shared and extended share handlers.

/// A share belongs to the currently installed template generation only when its
/// wire job id exactly matches the template id. Backend failover always emits a
/// new template id, even when both daemons are on the same tip.
pub fn is_current_job(job_id: u32, template_id: u64) -> bool {
    u64::from(job_id) == template_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prior_generation_is_stale() {
        assert!(is_current_job(42, 42));
        assert!(!is_current_job(41, 42));
        assert!(!is_current_job(42, 43));
    }

    #[test]
    fn never_truncates_a_u64_template_id() {
        assert!(!is_current_job(0, u64::from(u32::MAX) + 1));
    }
}
