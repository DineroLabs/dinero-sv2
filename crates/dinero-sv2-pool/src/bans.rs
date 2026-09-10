//! Temporary bans.
//!
//! An operator running the server should be able to stop serving someone
//! now, without a firewall rule and without a redeploy. This is that
//! mechanism, and it is deliberately narrow:
//!
//!   * **A ban declines future work. It never touches the PPLNS window.**
//!     Shares already contributed stay in the window and are still paid
//!     on the next block. The moment a ban also removes credit, "you
//!     never hold your miners' coins" stops being true, and that
//!     sentence is the whole reason this pool design is worth running.
//!     Refusing to serve someone and confiscating what they already
//!     earned are different acts, and only the first is the operator's
//!     to make. This module has no dependency on the window type, so
//!     that separation is structural rather than a convention someone
//!     has to remember.
//!
//!   * **Every ban expires.** A duration is required and capped at
//!     `MAX_BAN_SECS`. There is no permanent ban and no list to curate:
//!     an operator cannot leave someone blocked by forgetting about
//!     them, and cannot fat-finger a ban that outlives their memory of
//!     why they set it.
//!
//! Evasion is possible — identity is the payout script, and a new one is
//! free — but it is not free of consequence. PPLNS weight is keyed by
//! that same script, so rotating to evade a ban resets the miner's
//! window weight to zero and starts the ramp again. On a four-hour
//! window that is four hours of accrual discarded per evasion, and
//! repeated bans compound it. That is what makes a temporary ban worth
//! having even though it can be worked around.
//!
//! `now` is passed in rather than read from the clock, so expiry is
//! testable without sleeping.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Shortest ban worth issuing. Zero is refused rather than treated as an
/// unban: `/unban` exists for that, and silently overloading a duration
/// of zero is how an operator un-bans someone they meant to ban.
pub const MIN_BAN_SECS: u64 = 1;

/// Longest ban this pool will hold: one day. Bans are an operator
/// stopping abuse in progress, not a sentence. Anything needing longer
/// than a day is a decision to make again tomorrow, deliberately.
pub const MAX_BAN_SECS: u64 = 24 * 60 * 60;

/// One active ban, as reported on `/status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BanStatus {
    /// Hex of the banned payout script.
    pub target_hex: String,
    /// Wall-clock expiry, so a consumer can render an absolute time.
    pub expires_at_unix: u64,
    /// Seconds left, so a consumer does not have to trust its own clock
    /// against the pool's.
    pub expires_in_secs: u64,
}

/// Why a ban request was refused. Carried back to the operator verbatim
/// rather than collapsed into "bad request", because the three cases
/// have different fixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BanError {
    /// Duration below `MIN_BAN_SECS` — including zero.
    TooShort,
    /// Duration above `MAX_BAN_SECS`.
    TooLong,
    /// Target was not a decodable payout script.
    BadTarget,
}

impl BanError {
    pub fn message(&self) -> String {
        match self {
            BanError::TooShort => {
                format!("ban duration must be at least {MIN_BAN_SECS}s; use /unban to lift a ban")
            }
            BanError::TooLong => format!(
                "ban duration must not exceed {MAX_BAN_SECS}s ({}h); bans are temporary by design",
                MAX_BAN_SECS / 3600
            ),
            BanError::BadTarget => {
                "target must be a hex payout script, as reported in /status miners[].payout_script_hex"
                    .to_string()
            }
        }
    }
}

/// Active bans, keyed by payout script.
#[derive(Debug, Default)]
pub struct BanList {
    inner: Mutex<HashMap<Vec<u8>, u64>>,
}

impl BanList {
    /// Ban `target` for `seconds` from `now`. Re-banning an already
    /// banned target REPLACES its expiry rather than extending it, so an
    /// operator issuing a shorter ban gets the shorter ban they asked
    /// for instead of a silently longer one.
    pub fn ban(&self, target: Vec<u8>, seconds: u64, now: u64) -> Result<u64, BanError> {
        if target.is_empty() {
            return Err(BanError::BadTarget);
        }
        if seconds < MIN_BAN_SECS {
            return Err(BanError::TooShort);
        }
        if seconds > MAX_BAN_SECS {
            return Err(BanError::TooLong);
        }
        let expires_at = now.saturating_add(seconds);
        self.inner
            .lock()
            .expect("ban list mutex")
            .insert(target, expires_at);
        Ok(expires_at)
    }

    /// Lift a ban early. Returns whether one was actually lifted, so the
    /// operator can tell "done" from "there was nothing there" — the
    /// second usually means a mistyped target.
    pub fn unban(&self, target: &[u8]) -> bool {
        self.inner
            .lock()
            .expect("ban list mutex")
            .remove(target)
            .is_some()
    }

    /// Whether `target` is banned at `now`. An expired entry is not a
    /// ban, whether or not it has been pruned yet.
    pub fn is_banned(&self, target: &[u8], now: u64) -> bool {
        match self.inner.lock().expect("ban list mutex").get(target) {
            Some(expires_at) => *expires_at > now,
            None => false,
        }
    }

    /// Active bans at `now`, newest expiry last, with expired entries
    /// dropped from the map on the way past. Pruning here rather than on
    /// a timer means the list cannot grow without someone looking at it.
    pub fn active(&self, now: u64) -> Vec<BanStatus> {
        let mut guard = self.inner.lock().expect("ban list mutex");
        guard.retain(|_, expires_at| *expires_at > now);
        let mut out: Vec<BanStatus> = guard
            .iter()
            .map(|(target, expires_at)| BanStatus {
                target_hex: hex::encode(target),
                expires_at_unix: *expires_at,
                expires_in_secs: expires_at.saturating_sub(now),
            })
            .collect();
        out.sort_by(|a, b| {
            a.expires_at_unix
                .cmp(&b.expires_at_unix)
                .then_with(|| a.target_hex.cmp(&b.target_hex))
        });
        out
    }
}

/// The wire error code sent to a banned miner. Distinct from the other
/// rejection codes so their logs say why they stopped being served,
/// rather than leaving them to guess at a silent connection.
pub const BANNED_ERROR_CODE: &str = "banned";

/// The enforcement decision for one submitted share.
///
/// A named function rather than an inline `if`, so the rule is stated
/// once and testable without standing up a Noise session: the check runs
/// BEFORE any credit is recorded, and returns only a refusal code — it
/// has no access to the window and so cannot revoke anything already
/// earned.
pub fn share_refusal(bans: &BanList, payout_script: &[u8], now: u64) -> Option<&'static str> {
    bans.is_banned(payout_script, now)
        .then_some(BANNED_ERROR_CODE)
}

/// Decode a hex target from the wire. Kept here so the route and the
/// enforcement point agree on what a target is.
pub fn decode_target(hex_str: &str) -> Result<Vec<u8>, BanError> {
    let trimmed = hex_str.trim();
    if trimmed.is_empty() || trimmed.len() % 2 != 0 {
        return Err(BanError::BadTarget);
    }
    hex::decode(trimmed).map_err(|_| BanError::BadTarget)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000;
    fn script(b: u8) -> Vec<u8> {
        vec![0x51, 0x20, b]
    }

    #[test]
    fn a_ban_holds_until_its_expiry_then_lapses() {
        let bans = BanList::default();
        let expires = bans.ban(script(1), 3600, T0).expect("ban accepted");
        assert_eq!(expires, T0 + 3600);
        assert!(bans.is_banned(&script(1), T0));
        assert!(bans.is_banned(&script(1), T0 + 3599));
        // Expiry is exclusive: at the stroke, the ban is over.
        assert!(!bans.is_banned(&script(1), T0 + 3600));
        assert!(!bans.is_banned(&script(1), T0 + 4000));
    }

    #[test]
    fn a_ban_applies_only_to_its_target() {
        let bans = BanList::default();
        bans.ban(script(1), 3600, T0).unwrap();
        assert!(bans.is_banned(&script(1), T0));
        assert!(!bans.is_banned(&script(2), T0));
    }

    #[test]
    fn nothing_is_banned_by_default() {
        let bans = BanList::default();
        assert!(!bans.is_banned(&script(1), T0));
        assert!(bans.active(T0).is_empty());
    }

    // Zero is refused rather than silently meaning "unban". An operator
    // who fat-fingers a duration should get an error, not the opposite
    // of what they typed.
    #[test]
    fn a_zero_duration_is_refused_not_treated_as_an_unban() {
        let bans = BanList::default();
        bans.ban(script(1), 3600, T0).unwrap();
        assert_eq!(bans.ban(script(1), 0, T0), Err(BanError::TooShort));
        assert!(bans.is_banned(&script(1), T0), "the standing ban survives");
    }

    // "Temporary" is enforced, not merely documented: the cap is a day.
    #[test]
    fn a_duration_beyond_a_day_is_refused() {
        let bans = BanList::default();
        assert_eq!(bans.ban(script(1), MAX_BAN_SECS, T0), Ok(T0 + MAX_BAN_SECS));
        assert_eq!(
            bans.ban(script(2), MAX_BAN_SECS + 1, T0),
            Err(BanError::TooLong)
        );
        assert_eq!(
            bans.ban(script(3), 365 * 24 * 3600, T0),
            Err(BanError::TooLong)
        );
        assert!(!bans.is_banned(&script(2), T0));
    }

    // Replacing, not extending: an operator shortening a ban gets the
    // shorter ban rather than the longer one they were trying to undo.
    #[test]
    fn rebanning_replaces_the_expiry_in_both_directions() {
        let bans = BanList::default();
        bans.ban(script(1), 3600, T0).unwrap();
        bans.ban(script(1), 60, T0).unwrap();
        assert!(!bans.is_banned(&script(1), T0 + 61));

        bans.ban(script(1), 7200, T0).unwrap();
        assert!(bans.is_banned(&script(1), T0 + 7199));
    }

    #[test]
    fn unban_lifts_early_and_reports_whether_it_did() {
        let bans = BanList::default();
        bans.ban(script(1), 3600, T0).unwrap();
        assert!(bans.unban(&script(1)));
        assert!(!bans.is_banned(&script(1), T0));
        // A second unban found nothing — usually a mistyped target, and
        // the operator should be told rather than reassured.
        assert!(!bans.unban(&script(1)));
    }

    #[test]
    fn active_reports_remaining_time_and_drops_expired_entries() {
        let bans = BanList::default();
        bans.ban(script(1), 60, T0).unwrap();
        bans.ban(script(2), 3600, T0).unwrap();

        let active = bans.active(T0 + 30);
        assert_eq!(active.len(), 2);
        // Soonest expiry first.
        assert_eq!(active[0].target_hex, hex::encode(script(1)));
        assert_eq!(active[0].expires_in_secs, 30);
        assert_eq!(active[1].expires_in_secs, 3570);
        assert_eq!(active[1].expires_at_unix, T0 + 3600);

        let later = bans.active(T0 + 120);
        assert_eq!(later.len(), 1, "the lapsed ban is gone");
        assert_eq!(later[0].target_hex, hex::encode(script(2)));
    }

    #[test]
    fn an_empty_target_is_refused() {
        let bans = BanList::default();
        assert_eq!(bans.ban(Vec::new(), 3600, T0), Err(BanError::BadTarget));
    }

    #[test]
    fn targets_decode_from_the_hex_status_reports() {
        assert_eq!(decode_target("512001"), Ok(script(1)));
        assert_eq!(decode_target("  512001  "), Ok(script(1)));
        assert_eq!(decode_target(""), Err(BanError::BadTarget));
        assert_eq!(decode_target("51200"), Err(BanError::BadTarget));
        assert_eq!(decode_target("zzzz"), Err(BanError::BadTarget));
    }

    #[test]
    fn a_banned_script_has_its_shares_refused_with_a_named_code() {
        let bans = BanList::default();
        bans.ban(script(1), 3600, T0).unwrap();

        assert_eq!(
            share_refusal(&bans, &script(1), T0),
            Some(BANNED_ERROR_CODE)
        );
        assert_eq!(share_refusal(&bans, &script(2), T0), None);
        // And it lapses with the ban, without anyone lifting it.
        assert_eq!(share_refusal(&bans, &script(1), T0 + 3600), None);
    }

    // The rule the whole feature rests on. A ban is a refusal to serve,
    // never a clawback — so nothing in this module can reach the window,
    // and a ban changes no credit that already exists.
    #[test]
    fn banning_touches_no_earned_credit() {
        use crate::accounting::PplnsWindow;

        let mut window = PplnsWindow::new(3600);
        window.record(script(1), 1_000, T0);
        window.record(script(2), 1_000, T0);
        let before = window.weights();

        let bans = BanList::default();
        bans.ban(script(1), 3600, T0).unwrap();

        assert!(bans.is_banned(&script(1), T0));
        assert_eq!(
            window.weights(),
            before,
            "a banned contributor keeps every share they already earned"
        );
        assert_eq!(window.miner_bps(&script(1)), 5_000);
    }
}
