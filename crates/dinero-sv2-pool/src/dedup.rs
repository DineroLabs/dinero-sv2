//! Global accepted-share dedup.
//!
//! A share's header hash must be credited AT MOST ONCE, pool-wide:
//! resubmitting the same share on one channel would farm PPLNS weight
//! for free, and (until every work source is per-channel unique) two
//! channels grinding identical work can find the identical share —
//! observed live 2026-08-21, the same hash+nonce accepted from three
//! payout scripts. Bounded memory: oldest entries are evicted FIFO once
//! `cap` is reached (far beyond any plausible live share window).

use std::collections::{HashSet, VecDeque};

#[derive(Debug)]
pub struct ShareDedup {
    cap: usize,
    seen: HashSet<[u8; 32]>,
    order: VecDeque<[u8; 32]>,
}

impl ShareDedup {
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0, "dedup cap must be non-zero");
        Self {
            cap,
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// Record `hash`. Returns `true` if it was fresh (credit it),
    /// `false` if it was already seen (reject as duplicate).
    pub fn insert(&mut self, hash: [u8; 32]) -> bool {
        if self.seen.contains(&hash) {
            return false;
        }
        if self.seen.len() == self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.seen.insert(hash);
        self.order.push_back(hash);
        true
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn first_insert_is_fresh_second_is_duplicate() {
        let mut d = ShareDedup::new(16);
        assert!(d.insert(h(1)));
        assert!(!d.insert(h(1)));
        // A different hash is still fresh.
        assert!(d.insert(h(2)));
        assert!(!d.insert(h(2)));
    }

    #[test]
    fn evicts_oldest_beyond_cap() {
        let mut d = ShareDedup::new(2);
        assert!(d.insert(h(1)));
        assert!(d.insert(h(2)));
        assert!(d.insert(h(3))); // evicts h(1)
        assert_eq!(d.len(), 2);
        // h(1) was evicted → treated as fresh again; h(3) still known.
        assert!(d.insert(h(1)));
        assert!(!d.insert(h(3)));
    }

    #[test]
    fn len_stays_bounded() {
        let mut d = ShareDedup::new(8);
        for i in 0..64u8 {
            d.insert(h(i));
        }
        assert_eq!(d.len(), 8);
    }
}
