//! Pure stale-job rule shared by solo, shared and extended share handlers.

/// A share belongs to the currently installed template generation only when its
/// wire job id exactly matches the template id. Backend failover always emits a
/// new template id, even when both daemons are on the same tip.
pub fn is_current_job(job_id: u32, template_id: u64) -> bool {
    u64::from(job_id) == template_id
}

/// Failed channel derivation may retain a previously sent job. It remains usable
/// across same-parent refreshes, but must never earn credit after a new parent
/// or backend generation has been observed.
pub fn is_current_context(
    installed_epoch: u64,
    installed_parent: &[u8; 32],
    observed_epoch: u64,
    observed_parent: &[u8; 32],
) -> bool {
    installed_epoch == observed_epoch && installed_parent == observed_parent
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
    fn failed_refresh_cannot_keep_crediting_a_stale_context() {
        let parent = [1; 32];
        assert!(is_current_context(7, &parent, 7, &parent)); // PPLNS-only refresh
        assert!(!is_current_context(7, &parent, 7, &[2; 32])); // new chain parent
        assert!(!is_current_context(7, &parent, 8, &parent)); // same-tip failover
        assert!(is_current_context(8, &[2; 32], 8, &[2; 32])); // replacement installed
    }

    #[test]
    fn never_truncates_a_u64_template_id() {
        assert!(!is_current_job(0, u64::from(u32::MAX) + 1));
    }
}
