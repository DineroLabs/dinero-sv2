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
    let fee_una = (u128::from(p.reward_una) * u128::from(p.fee_bps) / 10_000) as u64;
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
            if v >= p.dust_una {
                outs.push(CoinbaseOutput { value_una: v, script_pubkey: (*script).clone() });
                paid += v;
            }
        }
    }
    if outs.is_empty() {
        // Empty window (or everything dusted): finder takes the pot.
        outs.push(CoinbaseOutput { value_una: pot, script_pubkey: p.finder_script.to_vec() });
        paid = pot;
    }

    // Remainder (rounding + dusted slices) → finder's output if present,
    // else the fee output.
    let remainder = pot - paid;
    if remainder > 0 {
        if let Some(f) = outs.iter_mut().find(|o| o.script_pubkey == p.finder_script) {
            f.value_una += remainder;
        } else {
            // absorbed by fee below
        }
    }
    let fee_total = if remainder > 0
        && !outs.iter().any(|o| o.script_pubkey == p.finder_script)
    {
        fee_una + remainder
    } else {
        fee_una
    };
    outs.push(CoinbaseOutput { value_una: fee_total, script_pubkey: p.fee_script.to_vec() });

    debug_assert_eq!(outs.iter().map(|o| o.value_una).sum::<u64>(), p.reward_una);
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
}
