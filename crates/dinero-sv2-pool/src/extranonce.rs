//! Per-channel coinbase-scriptSig extranonce injection.
//!
//! Every standard-share (non-JD) channel must mine a UNIQUE header, or
//! all channels race over the identical nonce space and the pool's
//! aggregate hashrate collapses to the fastest single miner (observed
//! live 2026-08-21: the same share hash+nonce accepted from three
//! different payout scripts). The daemon-blessed variation point is the
//! coinbase scriptSig: consensus allows 2–100 bytes with the BIP34
//! height push first (src/consensus/tx_validation.cpp), and appending
//! bytes there changes the coinbase txid → header merkle_root AND the
//! coinbase utreexo leaves → header utreexo_root. (The mandatory 8-byte
//! coinbase WITNESS "extranonce" is useless for this: witnesses are
//! excluded from txid, and coinbase-only DNRW uses the zero-wtxid
//! convention, so no header field would change.)

use anyhow::{anyhow, Result};

/// Consensus cap on the coinbase scriptSig (BIP34 rule mirrored from
/// dinerod's tx_validation.cpp: "Coinbase scriptSig must be 2-100
/// bytes").
const MAX_COINBASE_SCRIPTSIG: usize = 100;

/// Rewrite a stripped-coinbase PREFIX (version + input-count varint +
/// the single coinbase input + sequence — the exact layout
/// `PoolTemplate::coinbase_prefix` carries) so the coinbase scriptSig
/// gains a trailing 5-byte push of the little-endian `extranonce`
/// (`0x04 e0 e1 e2 e3`). Everything else is preserved byte-for-byte.
///
/// Errors on malformed prefixes (truncated, input count ≠ 1, trailing
/// garbage) and when the appended push would exceed the consensus
/// 100-byte scriptSig cap.
pub fn inject_scriptsig_extranonce(coinbase_prefix: &[u8], extranonce: u32) -> Result<Vec<u8>> {
    let p = coinbase_prefix;
    let need = |at: usize, n: usize| -> Result<()> {
        if at.checked_add(n).map_or(true, |end| end > p.len()) {
            Err(anyhow!("coinbase prefix truncated at byte {at}"))
        } else {
            Ok(())
        }
    };
    let mut i = 0usize;
    need(i, 4)?; // version
    i += 4;
    let (input_count, n) = read_varint(p, i)?;
    if input_count != 1 {
        return Err(anyhow!("coinbase must have exactly 1 input, got {input_count}"));
    }
    i += n;
    need(i, 36)?; // prevout txid + index
    i += 36;
    let (scriptsig_len, n) = read_varint(p, i)?;
    let scriptsig_len_pos = i;
    i += n;
    let scriptsig_len = usize::try_from(scriptsig_len)
        .map_err(|_| anyhow!("absurd scriptSig length {scriptsig_len}"))?;
    need(i, scriptsig_len)?;
    let scriptsig_end = i + scriptsig_len;
    need(scriptsig_end, 4)?; // sequence
    if scriptsig_end + 4 != p.len() {
        return Err(anyhow!(
            "coinbase prefix has trailing bytes after the input sequence"
        ));
    }

    let new_len = scriptsig_len + 5;
    if new_len > MAX_COINBASE_SCRIPTSIG {
        return Err(anyhow!(
            "extranonce would grow coinbase scriptSig to {new_len} bytes (consensus cap {MAX_COINBASE_SCRIPTSIG})"
        ));
    }

    let mut out = Vec::with_capacity(p.len() + 5);
    out.extend_from_slice(&p[..scriptsig_len_pos]);
    // new_len ≤ 100 always fits a 1-byte varint.
    out.push(new_len as u8);
    out.extend_from_slice(&p[scriptsig_len_pos + n..scriptsig_end]);
    out.push(0x04);
    out.extend_from_slice(&extranonce.to_le_bytes());
    out.extend_from_slice(&p[scriptsig_end..]); // sequence
    Ok(out)
}

/// Read a Bitcoin-style compactsize varint at `at`. Returns
/// (value, encoded length).
fn read_varint(p: &[u8], at: usize) -> Result<(u64, usize)> {
    let take = |n: usize| -> Result<&[u8]> {
        p.get(at + 1..at + 1 + n)
            .ok_or_else(|| anyhow!("varint truncated at byte {at}"))
    };
    match p.get(at) {
        None => Err(anyhow!("varint truncated at byte {at}")),
        Some(&b) if b < 0xfd => Ok((b as u64, 1)),
        Some(&0xfd) => Ok((
            u16::from_le_bytes(take(2)?.try_into().unwrap()) as u64,
            3,
        )),
        Some(&0xfe) => Ok((
            u32::from_le_bytes(take(4)?.try_into().unwrap()) as u64,
            5,
        )),
        Some(&0xff) => Ok((u64::from_le_bytes(take(8)?.try_into().unwrap()), 9)),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// version + in_count(1) + null prevout txid + 0xffffffff index +
    /// scriptSig [len 4: 0x03 aa bb cc] + sequence — the same shape the
    /// daemon's getblocktemplate coinbase produces.
    fn fixture_prefix() -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_le_bytes()); // version
        p.push(0x01); // input count
        p.extend_from_slice(&[0u8; 32]); // prevout txid
        p.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // prevout index
        p.push(0x04); // scriptSig len
        p.extend_from_slice(&[0x03, 0xaa, 0xbb, 0xcc]); // BIP34-ish height push
        p.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // sequence
        p
    }

    #[test]
    fn appends_pushed_extranonce_and_updates_length() {
        let prefix = fixture_prefix();
        let out = inject_scriptsig_extranonce(&prefix, 0x1234_5678).unwrap();
        // 5 bytes longer overall: the pushed extranonce.
        assert_eq!(out.len(), prefix.len() + 5);
        // Everything before the scriptSig length byte unchanged.
        assert_eq!(&out[..41], &prefix[..41]);
        // scriptSig length bumped 4 → 9.
        assert_eq!(out[41], 0x09);
        // Original scriptSig bytes preserved…
        assert_eq!(&out[42..46], &prefix[42..46]);
        // …then the push: 0x04 + LE extranonce.
        assert_eq!(&out[46..51], &[0x04, 0x78, 0x56, 0x34, 0x12]);
        // Sequence preserved as the trailing 4 bytes.
        assert_eq!(&out[51..], &prefix[46..]);
    }

    #[test]
    fn distinct_extranonces_produce_distinct_prefixes() {
        let prefix = fixture_prefix();
        let a = inject_scriptsig_extranonce(&prefix, 1).unwrap();
        let b = inject_scriptsig_extranonce(&prefix, 2).unwrap();
        assert_ne!(a, b);
        // Deterministic for the same extranonce.
        assert_eq!(a, inject_scriptsig_extranonce(&prefix, 1).unwrap());
    }

    #[test]
    fn rejects_scriptsig_overflowing_consensus_cap() {
        // 96-byte scriptSig: +5 would exceed the 100-byte cap.
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_le_bytes());
        p.push(0x01);
        p.extend_from_slice(&[0u8; 32]);
        p.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        p.push(96);
        p.extend_from_slice(&[0x51u8; 96]);
        p.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        let err = inject_scriptsig_extranonce(&p, 7).unwrap_err();
        assert!(err.to_string().contains("scriptSig"), "got: {err}");
    }

    #[test]
    fn rejects_truncated_prefix() {
        let prefix = fixture_prefix();
        assert!(inject_scriptsig_extranonce(&prefix[..prefix.len() - 2], 7).is_err());
    }

    #[test]
    fn rejects_non_single_input_coinbase() {
        let mut p = fixture_prefix();
        p[4] = 0x02; // input count
        assert!(inject_scriptsig_extranonce(&p, 7).is_err());
    }

    #[test]
    fn rejects_trailing_garbage_after_sequence() {
        let mut p = fixture_prefix();
        p.push(0x00);
        assert!(inject_scriptsig_extranonce(&p, 7).is_err());
    }
}
