//! Trustless PPLNS coinbase split. Pure math — no I/O, no custody.
//! Dust and over-cap contributors are simply not paid THIS block;
//! their window credit remains and pays out from future blocks.

use std::collections::HashMap;

use dinero_sv2_jd::CoinbaseOutput;

pub struct SplitParams<'a> {
    pub reward_una: u64,
    pub fee_bps: u32,
    pub fee_script: &'a [u8],
    pub max_outputs: usize,
    pub dust_una: u64,
    pub finder_script: &'a [u8],
}

pub fn compute_split(weights: &HashMap<Vec<u8>, u128>, p: &SplitParams) -> Vec<CoinbaseOutput> {
    // fee_bps is caller-controlled input; clamp so it can never exceed
    // 100% and underflow `p.reward_una - fee_una` below.
    let fee_bps = p.fee_bps.min(10_000);
    let fee_una = (u128::from(p.reward_una) * u128::from(fee_bps) / 10_000) as u64;
    let pot = p.reward_una - fee_una;

    // Deterministic order: weight desc, then script asc.
    let mut ranked: Vec<(&Vec<u8>, u128)> =
        weights.iter().map(|(k, v)| (k, *v)).filter(|(_, w)| *w > 0).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked.truncate(p.max_outputs);

    let elected_total: u128 = ranked.iter().map(|(_, w)| *w).sum();

    let mut outs: Vec<CoinbaseOutput> = Vec::new();
    let mut paid: u64 = 0;
    if elected_total > 0 {
        for (script, w) in &ranked {
            let v = ((u128::from(pot) * w) / elected_total) as u64;
            if v > 0 && v >= p.dust_una {
                outs.push(CoinbaseOutput { value_una: v, script_pubkey: (*script).clone() });
                paid += v;
            }
        }
    }
    if outs.is_empty() && pot > 0 {
        // Empty window (or everything dusted): finder takes the pot.
        outs.push(CoinbaseOutput { value_una: pot, script_pubkey: p.finder_script.to_vec() });
        paid = pot;
    }

    // Remainder (rounding + dusted slices) → finder's output if present;
    // else the fee output if it will be nonzero; else (fee_bps == 0, so
    // there is no fee output to absorb it) the largest contributor.
    let remainder = pot - paid;
    let mut remainder_absorbed = false;
    if remainder > 0 {
        if let Some(f) = outs.iter_mut().find(|o| o.script_pubkey == p.finder_script) {
            f.value_una += remainder;
            remainder_absorbed = true;
        } else if fee_una == 0 {
            if let Some(top) = outs.first_mut() {
                top.value_una += remainder;
                remainder_absorbed = true;
            }
        }
    }
    let fee_total = if remainder > 0 && !remainder_absorbed { fee_una + remainder } else { fee_una };
    // Never emit a zero-value output.
    if fee_total > 0 {
        outs.push(CoinbaseOutput { value_una: fee_total, script_pubkey: p.fee_script.to_vec() });
    }

    assert_eq!(
        outs.iter().map(|o| o.value_una).sum::<u64>(),
        p.reward_una,
        "split invariant violated: outputs != reward"
    );
    outs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: u8) -> Vec<u8> {
        vec![0x51, 0x20, b]
    }

    #[test]
    fn split_sums_to_reward_with_fee() {
        let mut w = HashMap::new();
        w.insert(s(1), 300u128);
        w.insert(s(2), 100u128);
        let p = SplitParams {
            reward_una: 10_000_000_000,
            fee_bps: 200,
            fee_script: &s(9),
            max_outputs: 20,
            dust_una: 10_000,
            finder_script: &s(1),
        };
        let outs = compute_split(&w, &p);
        assert_eq!(outs.iter().map(|o| o.value_una).sum::<u64>(), 10_000_000_000);
        let fee = outs.iter().find(|o| o.script_pubkey == s(9)).unwrap();
        assert!(fee.value_una >= 200_000_000); // ≥2% (may absorb rounding)
        let a = outs.iter().find(|o| o.script_pubkey == s(1)).unwrap();
        let b = outs.iter().find(|o| o.script_pubkey == s(2)).unwrap();
        assert!(a.value_una > b.value_una * 2); // ~3:1 plus rounding to finder
    }

    #[test]
    fn split_caps_outputs_and_drops_dust() {
        let mut w = HashMap::new();
        for i in 0..30 {
            w.insert(s(i), 100u128);
        }
        w.insert(s(200), 1u128); // will be dust
        let p = SplitParams {
            reward_una: 10_000_000_000,
            fee_bps: 200,
            fee_script: &s(255),
            max_outputs: 20,
            dust_una: 10_000_000,
            finder_script: &s(0),
        };
        let outs = compute_split(&w, &p);
        // ≤ 20 contributor outputs + 1 fee output:
        assert!(outs.len() <= 21);
        assert_eq!(outs.iter().map(|o| o.value_una).sum::<u64>(), 10_000_000_000);
        assert!(!outs.iter().any(|o| o.script_pubkey == s(200)));
    }

    #[test]
    fn empty_window_pays_finder_minus_fee() {
        let w = HashMap::new();
        let p = SplitParams {
            reward_una: 10_000_000_000,
            fee_bps: 200,
            fee_script: &s(9),
            max_outputs: 20,
            dust_una: 10_000,
            finder_script: &s(7),
        };
        let outs = compute_split(&w, &p);
        assert_eq!(outs.len(), 2);
        assert_eq!(
            outs.iter().find(|o| o.script_pubkey == s(7)).unwrap().value_una,
            9_800_000_000
        );
        assert_eq!(
            outs.iter().find(|o| o.script_pubkey == s(9)).unwrap().value_una,
            200_000_000
        );
    }

    /// Every output's value_una must be > 0, and the outputs must sum
    /// exactly to the reward. Shared by every test below (F3).
    fn assert_split_invariant(outs: &[CoinbaseOutput], reward_una: u64) {
        for o in outs {
            assert!(o.value_una > 0, "zero-value output emitted: {:?}", o.script_pubkey);
        }
        assert_eq!(outs.iter().map(|o| o.value_una).sum::<u64>(), reward_una);
    }

    #[test]
    fn fee_bps_over_10000_is_clamped() {
        // fee_bps=25_000 (250%) must clamp to 10_000 (100%): the whole
        // reward becomes fee, pot=0, and no phantom zero-value finder
        // output is emitted.
        let mut w = HashMap::new();
        w.insert(s(1), 300u128);
        w.insert(s(2), 100u128);
        let p = SplitParams {
            reward_una: 1_000,
            fee_bps: 25_000,
            fee_script: &s(9),
            max_outputs: 20,
            dust_una: 10,
            finder_script: &s(7),
        };
        let outs = compute_split(&w, &p);
        assert_split_invariant(&outs, 1_000);
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].script_pubkey, s(9));
        assert_eq!(outs[0].value_una, 1_000);
    }

    #[test]
    fn elected_but_dusted_contributor_is_skipped() {
        // B survives top-N truncation but its pro-rata slice is dust.
        // It gets no output; its slice (the rounding remainder) lands
        // on the fee output since the finder isn't in the split.
        let mut w = HashMap::new();
        w.insert(s(1), 990u128); // A
        w.insert(s(2), 10u128); // B — will be dusted
        let p = SplitParams {
            reward_una: 10_000_000,
            fee_bps: 200,
            fee_script: &s(9),
            max_outputs: 20,
            dust_una: 200_000,
            finder_script: &s(7), // not in the split
        };
        let outs = compute_split(&w, &p);
        assert_split_invariant(&outs, 10_000_000);
        assert!(!outs.iter().any(|o| o.script_pubkey == s(2)), "dusted B must not appear");
        let a = outs.iter().find(|o| o.script_pubkey == s(1)).unwrap();
        assert_eq!(a.value_una, 9_702_000);
        let fee = outs.iter().find(|o| o.script_pubkey == s(9)).unwrap();
        assert_eq!(fee.value_una, 298_000); // 200_000 fee + 98_000 absorbed remainder
    }

    #[test]
    fn all_dusted_falls_back_to_finder() {
        // Nonempty weights, but every pro-rata slice is below dust:
        // the whole pot falls back to the finder, and the fee output
        // remains separate.
        let mut w = HashMap::new();
        w.insert(s(1), 1u128);
        w.insert(s(2), 1u128);
        let p = SplitParams {
            reward_una: 10_000_000_000,
            fee_bps: 200,
            fee_script: &s(9),
            max_outputs: 20,
            dust_una: 10_000_000_000, // larger than any possible slice
            finder_script: &s(7),
        };
        let outs = compute_split(&w, &p);
        assert_split_invariant(&outs, 10_000_000_000);
        assert_eq!(outs.len(), 2);
        assert_eq!(
            outs.iter().find(|o| o.script_pubkey == s(7)).unwrap().value_una,
            9_800_000_000
        );
        assert_eq!(
            outs.iter().find(|o| o.script_pubkey == s(9)).unwrap().value_una,
            200_000_000
        );
    }

    #[test]
    fn zero_fee_remainder_goes_to_largest_contributor() {
        // fee_bps=0: no fee output should be fabricated just to hold
        // rounding dust. When the finder isn't in the split and there's
        // no fee output to absorb the remainder, it goes to the largest
        // contributor's output instead.
        let mut w = HashMap::new();
        w.insert(s(1), 99u128); // A — largest
        w.insert(s(2), 1u128); // B — will be dusted
        let p = SplitParams {
            reward_una: 1_000,
            fee_bps: 0,
            fee_script: &s(9),
            max_outputs: 20,
            dust_una: 50,
            finder_script: &s(7), // not in the split
        };
        let outs = compute_split(&w, &p);
        assert_split_invariant(&outs, 1_000);
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].script_pubkey, s(1));
        assert_eq!(outs[0].value_una, 1_000);
    }
}
