//! Payout ownership checks for negotiated immutable solo jobs.
use crate::{assemble_stripped_coinbase, compute_root, CoinbaseOutput};
use dinero_sv2_common::{ImmutableCoinbase, NewTemplateDinero};

/// Check the exact solo payout and coinbase inclusion before starting work.
/// This does not claim independent validation of the block's other transactions.
pub fn verify_owned_job(
    message: &ImmutableCoinbase,
    job: &NewTemplateDinero,
    channel: u32,
    height: Option<u32>,
    payout: &[u8],
) -> Result<(), &'static str> {
    let context = &message.context;
    if message.template_id != job.template_id
        || context.channel_id != channel
        || height != Some(context.height)
        || context.state_commitment_root.is_some()
        || context.coinbase_value_una == 0
        || message.outputs.len() > 64
        || context.merkle_path.len() > 64
    {
        return Err("immutable solo context mismatch");
    }
    let prefix = &context.coinbase_prefix;
    if prefix.len() < 46
        || (prefix[..4] != 1u32.to_le_bytes() && prefix[..4] != 2u32.to_le_bytes())
        || prefix[4] != 1
        || prefix[5..37].iter().any(|b| *b != 0)
        || prefix[37..41] != [0xff; 4]
        || prefix[41] < 2
        || prefix[41] > 100
        || prefix.len() != 46 + usize::from(prefix[41])
        || context.coinbase_suffix.len() != 4
    {
        return Err("invalid solo coinbase input framing");
    }
    let script = &prefix[42..42 + usize::from(prefix[41])];
    let n = usize::from(script[0]);
    if !(1..=5).contains(&n) || n + 1 > script.len() || script[n] & 0x80 != 0 {
        return Err("invalid solo coinbase height");
    }
    let mut encoded_height = 0u64;
    for (i, byte) in script[1..=n].iter().enumerate() {
        encoded_height |= u64::from(*byte) << (8 * i);
    }
    if encoded_height != u64::from(context.height) {
        return Err("solo coinbase height mismatch");
    }
    let mut total = 0u64;
    let mut paid = 0;
    let mut commitments = [false; 3];
    let mut outputs = Vec::new();
    for output in &message.outputs {
        total = total
            .checked_add(output.value_una)
            .ok_or("solo amount overflow")?;
        let script = &output.script_pubkey;
        if script.first() == Some(&0x6a) {
            if output.value_una != 0 || script.len() != 39 || script[1] != 37 {
                return Err("invalid solo commitment");
            }
            let index = match &script[2..7] {
                b"DNRS\x02" => 0,
                b"DNRW\x01" => 1,
                b"DNRF\x01" => 2,
                _ => return Err("unknown solo commitment"),
            };
            if commitments[index] {
                return Err("duplicate solo commitment");
            }
            commitments[index] = true;
        } else {
            if script != payout || output.value_una != context.coinbase_value_una {
                return Err("solo payout does not belong to miner");
            }
            paid += 1;
        }
        outputs.push(CoinbaseOutput {
            value_una: output.value_una,
            script_pubkey: script.clone(),
        });
    }
    if paid != 1 || total != context.coinbase_value_una || commitments != [true; 3] {
        return Err("incomplete solo coinbase");
    }
    let (_, id) = assemble_stripped_coinbase(prefix, &outputs, &context.coinbase_suffix);
    if compute_root(id, &context.merkle_path) != job.merkle_root {
        return Err("solo coinbase is not bound to header");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dinero_sv2_common::{CoinbaseContext, CoinbaseOutputWire};
    fn fixture() -> (ImmutableCoinbase, NewTemplateDinero, Vec<u8>) {
        let payout = [vec![0x51, 0x20], vec![7; 32]].concat();
        let mut prefix = 1u32.to_le_bytes().to_vec();
        prefix.push(1);
        prefix.extend([0; 32]);
        prefix.extend([0xff; 4]);
        prefix.extend([7, 1, 102, 4, 17, 0, 0, 0]);
        prefix.extend([0xff; 4]);
        let mut outputs = vec![CoinbaseOutputWire {
            value_una: 100,
            script_pubkey: payout.clone(),
        }];
        for (tag, version) in [(b"DNRS", 2), (b"DNRW", 1), (b"DNRF", 1)] {
            outputs.push(CoinbaseOutputWire {
                value_una: 0,
                script_pubkey: [vec![0x6a, 37], tag.to_vec(), vec![version], vec![9; 32]].concat(),
            });
        }
        let context = CoinbaseContext {
            channel_id: 3,
            coinbase_prefix: prefix,
            coinbase_suffix: vec![0; 4],
            merkle_path: vec![[4; 32]],
            height: 102,
            coinbase_value_una: 100,
            state_commitment_root: None,
        };
        let message = ImmutableCoinbase {
            template_id: 77,
            context,
            outputs,
        };
        let list = message
            .outputs
            .iter()
            .map(|o| CoinbaseOutput {
                value_una: o.value_una,
                script_pubkey: o.script_pubkey.clone(),
            })
            .collect::<Vec<_>>();
        let (_, id) = assemble_stripped_coinbase(
            &message.context.coinbase_prefix,
            &list,
            &message.context.coinbase_suffix,
        );
        let job = NewTemplateDinero {
            template_id: 77,
            future_template: false,
            version: 1,
            prev_block_hash: [1; 32],
            merkle_root: compute_root(id, &message.context.merkle_path),
            utreexo_root: [2; 32],
            timestamp: 100,
            difficulty: 0x207fffff,
            coinbase_outputs_commitment: [0; 32],
        };
        (message, job, payout)
    }
    #[test]
    fn accepts_exact_owned_solo_coinbase_with_merkle_branch() {
        let (message, job, payout) = fixture();
        assert!(verify_owned_job(&message, &job, 3, Some(102), &payout).is_ok());
    }
    #[test]
    fn rejects_redirected_payout_overflow_duplicates_and_wrong_job() {
        let (message, job, payout) = fixture();
        let mut bad = message.clone();
        bad.outputs[0].script_pubkey[3] ^= 1;
        assert!(verify_owned_job(&bad, &job, 3, Some(102), &payout).is_err());
        let mut bad = message.clone();
        bad.outputs[0].value_una = u64::MAX;
        assert!(verify_owned_job(&bad, &job, 3, Some(102), &payout).is_err());
        let mut bad = message.clone();
        bad.outputs.push(bad.outputs[1].clone());
        assert!(verify_owned_job(&bad, &job, 3, Some(102), &payout).is_err());
        let mut bad = message.clone();
        bad.context.merkle_path[0][0] ^= 1;
        assert!(verify_owned_job(&bad, &job, 3, Some(102), &payout).is_err());
        assert!(verify_owned_job(&message, &job, 4, Some(102), &payout).is_err());
        assert!(verify_owned_job(&message, &job, 3, Some(103), &payout).is_err());
        let mut bad = message.clone();
        bad.context.coinbase_prefix[43] = 103;
        assert!(verify_owned_job(&bad, &job, 3, Some(102), &payout).is_err());
        let mut bad = job;
        bad.template_id += 1;
        assert!(verify_owned_job(&message, &bad, 3, Some(102), &payout).is_err());
    }
}
