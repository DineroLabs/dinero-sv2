//! Immutable Orchard jobs: Core owns transaction selection and proof framing.
//! A derived PPLNS coinbase is finalized before miners hash its exact header.
use crate::{
    mapper::{self, PoolTemplate},
    rpc::RpcClient,
    shared_template::SharedTemplate,
    target::compact_to_target,
};
use anyhow::{anyhow, ensure, Context, Result};
use dinero_sv2_common::{sha256d, NewTemplateDinero, SubmitSharesDinero};
use dinero_sv2_jd::CoinbaseOutput;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct Origin {
    pub client: Arc<RpcClient>,
    pub job_id: String,
    pub address: String,
    pub transaction_count: u64,
    pub total_fees_una: u64,
    pub wire_bytes: Vec<u8>,
    pub merkle_path: Vec<[u8; 32]>,
    pub min_time: u64,
    pub max_time: u64,
}
impl std::fmt::Debug for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchardOrigin")
            .field("endpoint", &self.client.endpoint())
            .field("job_id", &self.job_id)
            .finish()
    }
}
fn number(job: &Value, key: &str) -> Result<u64> {
    job[key]
        .as_u64()
        .ok_or_else(|| anyhow!("invalid Orchard job {key}"))
}
pub fn map_job(
    job: &Value,
    template_id: u64,
    client: Arc<RpcClient>,
    address: String,
) -> Result<(PoolTemplate, Origin)> {
    ensure!(
        job["profile"] == "orchard-v1",
        "backend did not return an Orchard job"
    );
    let header = hex::decode(
        job["header_hex"]
            .as_str()
            .context("missing Orchard header")?,
    )?;
    ensure!(
        header.len() == 128 && header[116..].iter().all(|b| *b == 0),
        "invalid Orchard header frame"
    );
    let previous = header[4..36].iter().rev().copied().collect::<Vec<_>>();
    ensure!(
        job["prev_hash"].as_str() == Some(hex::encode(previous).as_str()),
        "Orchard parent/header mismatch"
    );
    let job_id = job["job_id"]
        .as_str()
        .context("missing Orchard job identity")?
        .to_owned();
    ensure!(
        !job_id.is_empty() && job_id.len() <= 128,
        "invalid Orchard job identity"
    );
    let height = u32::try_from(number(job, "height")?)?;
    let version = u32::from_le_bytes(header[0..4].try_into()?);
    let timestamp = u64::from_le_bytes(header[100..108].try_into()?);
    let difficulty = u32::from_le_bytes(header[108..112].try_into()?);
    let coinbase_full_hex = job["coinbase_hex"]
        .as_str()
        .context("missing Orchard coinbase")?
        .to_owned();
    let coinbase = hex::decode(&coinbase_full_hex)?;
    let (coinbase_prefix, coinbase_suffix, coinbase_witness_bytes) =
        mapper::split_coinbase_segwit(&coinbase_full_hex)?;
    let outputs = mapper::coinbase_outputs(&coinbase_full_hex)?;
    let value = outputs.iter().try_fold(0u64, |sum, o| {
        sum.checked_add(o.value_una)
            .context("coinbase value overflow")
    })?;
    ensure!(
        value == number(job, "coinbase_value_una")?,
        "Orchard coinbase amount mismatch"
    );
    let min_time = number(job, "min_time")?;
    let max_time = number(job, "max_time")?;
    ensure!(
        min_time == timestamp && max_time >= min_time,
        "invalid Orchard job time window"
    );
    let wire_bytes = match job["block_hex"].as_str() {
        Some(encoded) => {
            ensure!(
                encoded.len() <= 8_000_000,
                "Orchard block exceeds wire budget"
            );
            let bytes = hex::decode(encoded)?;
            ensure!(
                bytes.len() > 129 && bytes[..128] == header,
                "Orchard header/body mismatch"
            );
            let (count, size) = mapper::read_compact_size(&bytes, 128)?;
            ensure!(
                count == number(job, "transaction_count")?
                    && bytes[128 + size..].starts_with(&coinbase),
                "Orchard coinbase/body mismatch"
            );
            bytes
        }
        None => Vec::new(),
    };
    let merkle_path = job["coinbase_merkle_path"]
        .as_array()
        .map(|path| {
            path.iter()
                .map(|value| {
                    let bytes = hex::decode(value.as_str().context("invalid coinbase branch")?)?;
                    <[u8; 32]>::try_from(bytes).map_err(|_| anyhow!("invalid coinbase branch hash"))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    ensure!(merkle_path.len() <= 64, "coinbase branch exceeds budget");
    let origin = Origin {
        client,
        job_id,
        address,
        wire_bytes,
        merkle_path,
        transaction_count: number(job, "transaction_count")?,
        total_fees_una: number(job, "total_fees_una")?,
        min_time,
        max_time,
    };
    ensure!(origin.transaction_count >= 1, "missing Orchard coinbase");
    let wire = NewTemplateDinero {
        template_id,
        future_template: false,
        version,
        prev_block_hash: header[4..36].try_into()?,
        merkle_root: header[36..68].try_into()?,
        utreexo_root: header[68..100].try_into()?,
        timestamp,
        difficulty,
        coinbase_outputs_commitment: sha256d(&coinbase),
    };
    // Bodies remain opaque in Core. These compatibility fields MUST NOT be
    // passed to a historical/JD builder or to legacy submitblock assembly.
    let pt = PoolTemplate {
        wire,
        coinbase_full_hex,
        block_target: compact_to_target(difficulty),
        height,
        coinbase_value_una: value,
        utreexo_pre_block: None,
        coinbase_prefix,
        coinbase_suffix,
        coinbase_witness_bytes,
        merkle_path: Vec::new(),
        coinbase_txid_raw: [0; 32],
        mempool_txs: Vec::new(),
        spent_input_scripts: Vec::new(),
    };
    Ok((pt, origin))
}
pub async fn derive_shared(
    base: &Origin,
    pt: &PoolTemplate,
    outputs: Vec<CoinbaseOutput>,
    channel: u32,
) -> Result<SharedTemplate> {
    let request = json!({"address":base.address,"base_job_id":base.job_id,"coinbase_extranonce":channel,"include_block":true,
        "coinbase_outputs": outputs.iter().map(|o| json!({"script_pubkey_hex":hex::encode(&o.script_pubkey),"value_una":o.value_una})).collect::<Vec<_>>()});
    let result = base.client.call_raw("mining.getjob", request).await?;
    let (derived, origin) = map_job(
        &result,
        pt.wire.template_id,
        base.client.clone(),
        base.address.clone(),
    )?;
    ensure!(
        derived.height == pt.height
            && derived.wire.prev_block_hash == pt.wire.prev_block_hash
            && derived.wire.difficulty == pt.wire.difficulty
            && origin.transaction_count == base.transaction_count
            && origin.total_fees_una == base.total_fees_una,
        "derived Orchard job changed selected context"
    );
    let actual = mapper::coinbase_outputs(&derived.coinbase_full_hex)?;
    let payouts: Vec<_> = actual
        .iter()
        .filter(|o| o.script_pubkey.first() != Some(&0x6a))
        .collect();
    ensure!(
        payouts.len() == outputs.len()
            && payouts
                .iter()
                .zip(&outputs)
                .all(|(a, b)| a.value_una == b.value_una && a.script_pubkey == b.script_pubkey),
        "Core did not retain exact pool payouts"
    );
    Ok(SharedTemplate {
        wire: derived.wire,
        height: derived.height,
        coinbase_full_hex: derived.coinbase_full_hex,
        outputs: actual,
        mempool_tx_data: Vec::new(),
        orchard: Some(origin),
    })
}
impl Origin {
    pub fn accepts_header(&self, wire: &NewTemplateDinero, share: &SubmitSharesDinero) -> bool {
        share.version == wire.version
            && share.timestamp >= self.min_time
            && share.timestamp <= self.max_time
    }
    pub fn block_hex(&self, share: &SubmitSharesDinero) -> Result<String> {
        ensure!(self.wire_bytes.len() > 128, "missing sealed Orchard block");
        let mut wire = self.wire_bytes.clone();
        ensure!(
            share.version == u32::from_le_bytes(wire[..4].try_into()?)
                && share.timestamp >= self.min_time
                && share.timestamp <= self.max_time,
            "immutable Orchard header modified"
        );
        wire[100..108].copy_from_slice(&share.timestamp.to_le_bytes());
        wire[112..116].copy_from_slice(&share.nonce.to_le_bytes());
        Ok(hex::encode(wire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::Auth;
    use dinero_sv2_common::HeaderAssembly;
    fn fixture() -> (Value, Arc<RpcClient>) {
        let pt = mapper::tests::fixture_pool_template();
        let share = SubmitSharesDinero {
            channel_id: 1,
            sequence_number: 1,
            job_id: 42,
            nonce: 0,
            timestamp: pt.wire.timestamp,
            version: pt.wire.version,
        };
        let header = HeaderAssembly::bytes(&pt.wire, &share);
        let mut bytes = header.to_vec();
        bytes.push(1);
        bytes.extend(hex::decode(&pt.coinbase_full_hex).unwrap());
        bytes.extend([0x51; 32]); // Opaque suffix fixture, not a valid consensus proof.
        let previous = header[4..36].iter().rev().copied().collect::<Vec<_>>();
        let job = json!({"profile":"orchard-v1","header_hex":hex::encode(header),"prev_hash":hex::encode(previous),
            "job_id":"fixture","height":pt.height,"coinbase_hex":pt.coinbase_full_hex,
            "coinbase_value_una":pt.coinbase_value_una,"min_time":pt.wire.timestamp,"max_time":pt.wire.timestamp+30,
            "transaction_count":1,"total_fees_una":0,"block_hex":hex::encode(bytes),"coinbase_merkle_path":[]});
        (
            job,
            Arc::new(
                RpcClient::new(
                    "http://127.0.0.1:1".into(),
                    Auth::UserPass("fixture".into(), "fixture".into()),
                )
                .unwrap(),
            ),
        )
    }
    #[test]
    fn exact_body_is_retained_while_only_nonce_and_bounded_time_roll() {
        let (job, client) = fixture();
        let (pt, origin) = map_job(&job, 42, client, "fixture".into()).unwrap();
        let original = origin.wire_bytes.clone();
        let mut share = SubmitSharesDinero {
            channel_id: 1,
            sequence_number: 1,
            job_id: 42,
            nonce: 71,
            timestamp: pt.wire.timestamp + 1,
            version: pt.wire.version,
        };
        assert!(origin.accepts_header(&pt.wire, &share));
        let wire = hex::decode(origin.block_hex(&share).unwrap()).unwrap();
        assert_eq!(wire[128..], original[128..]);
        assert_eq!(origin.wire_bytes, original);
        assert_eq!(wire[..128], HeaderAssembly::bytes(&pt.wire, &share));
        share.version ^= 1;
        assert!(!origin.accepts_header(&pt.wire, &share));
        assert!(origin.block_hex(&share).is_err());
        share.version = pt.wire.version;
        share.timestamp = origin.max_time + 1;
        assert!(origin.block_hex(&share).is_err());
    }
    #[test]
    fn refuses_mismatched_parent_body_amount_and_branch() {
        let (job, client) = fixture();
        let mut bad = job.clone();
        bad["prev_hash"] = json!("00".repeat(32));
        assert!(map_job(&bad, 1, client.clone(), "fixture".into()).is_err());
        let mut bad = job.clone();
        bad["coinbase_value_una"] = json!(1);
        assert!(map_job(&bad, 1, client.clone(), "fixture".into()).is_err());
        let mut bad = job.clone();
        bad["coinbase_merkle_path"] = json!(["00"]);
        assert!(map_job(&bad, 1, client.clone(), "fixture".into()).is_err());
        let mut bad = job;
        let mut wire = hex::decode(bad["block_hex"].as_str().unwrap()).unwrap();
        wire[130] ^= 1;
        bad["block_hex"] = json!(hex::encode(wire));
        assert!(map_job(&bad, 1, client, "fixture".into()).is_err());
    }
}
