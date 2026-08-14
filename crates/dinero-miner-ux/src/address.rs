use serde::Serialize;

#[derive(Debug, PartialEq, Serialize)]
pub enum AddressError {
    Empty,
    Shielded,
    WrongNetwork,
    BadChecksum,
    Invalid,
    NotTaproot,
}

impl AddressError {
    pub fn message(&self) -> &'static str {
        match self {
            AddressError::Empty => "Enter a Dinero address.",
            AddressError::Shielded => {
                "Shielded (dins1…) addresses can't receive mining payouts. Use a transparent din1… address."
            }
            AddressError::WrongNetwork => {
                "That's a testnet/regtest address. Enter a mainnet din1… address."
            }
            AddressError::BadChecksum => "Checksum doesn't match — the address has a typo.",
            AddressError::Invalid => "Not a valid Dinero address.",
            AddressError::NotTaproot => {
                "Pool payouts need a Taproot (din1p…) address; this one is a different type."
            }
        }
    }
}

/// The 34-byte P2TR scriptPubKey (`OP_1 OP_PUSHBYTES_32 <program>`) as hex —
/// the form `dinero-sv2-miner --payout-script-hex` expects. Consensus sends
/// the block reward / PPLNS split to this script.
pub fn payout_script_hex(input: &str) -> Result<String, AddressError> {
    let s = validate_address(input)?;
    let (_hrp, version, program) =
        bech32::segwit::decode(&s).map_err(|_| AddressError::Invalid)?;
    if version != bech32::Fe32::P || program.len() != 32 {
        return Err(AddressError::NotTaproot);
    }
    Ok(format!("5120{}", hex::encode(program)))
}

pub fn validate_address(input: &str) -> Result<String, AddressError> {
    let s = input.trim().to_lowercase();
    if s.is_empty() {
        return Err(AddressError::Empty);
    }
    // HRP-specific rejections read the prefix before the LAST '1' separator.
    if let Some(idx) = s.rfind('1') {
        match &s[..idx] {
            "dins" => return Err(AddressError::Shielded),
            "tdin" | "rdin" => return Err(AddressError::WrongNetwork),
            _ => {}
        }
    }
    match bech32::decode(&s) {
        Ok((hrp, _data)) if hrp.as_str() == "din" => Ok(s),
        Ok(_) => Err(AddressError::Invalid),
        Err(e) => {
            // The bech32 crate reports checksum failures distinctly; everything
            // else (bad chars, mixed case, length) is Invalid.
            let msg = e.to_string().to_lowercase();
            if msg.contains("checksum") {
                Err(AddressError::BadChecksum)
            } else {
                Err(AddressError::Invalid)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Real mainnet addresses (throwaway sim wallets from 2026-08-12 session).
    const VALID_1: &str = "din1pafzgzwwfeqkfh7u4kkpe8qy97gey3zcvymx5eumxzx45m08q6tgqedz700";
    const VALID_2: &str = "din1p977z3vkm5a2skmvlfvng4lxd9mnv95z43a38pastawrnc89gu7xsfcyczw";

    #[test]
    fn accepts_real_mainnet_addresses() {
        assert_eq!(validate_address(VALID_1).unwrap(), VALID_1);
        assert_eq!(validate_address(VALID_2).unwrap(), VALID_2);
    }
    #[test]
    fn trims_and_lowercases() {
        let shouty = format!("  {}  ", VALID_1.to_uppercase());
        assert_eq!(validate_address(&shouty).unwrap(), VALID_1);
    }
    #[test]
    fn rejects_empty() {
        assert_eq!(validate_address("   "), Err(AddressError::Empty));
    }
    #[test]
    fn rejects_bad_checksum() {
        let mut s = VALID_1.to_string();
        s.pop();
        s.push('q'); // corrupt final checksum char
        assert_eq!(validate_address(&s), Err(AddressError::BadChecksum));
    }
    #[test]
    fn rejects_shielded_prefix() {
        assert_eq!(validate_address("dins1qqqqqq"), Err(AddressError::Shielded));
    }
    #[test]
    fn rejects_testnet_and_regtest() {
        assert_eq!(validate_address("tdin1qqqqqq"), Err(AddressError::WrongNetwork));
        assert_eq!(validate_address("rdin1qqqqqq"), Err(AddressError::WrongNetwork));
    }
    #[test]
    fn payout_script_is_34_byte_p2tr() {
        let hex1 = payout_script_hex(VALID_1).unwrap();
        assert_eq!(hex1.len(), 68, "34 bytes = 68 hex chars");
        assert!(hex1.starts_with("5120"), "OP_1 OP_PUSHBYTES_32 prefix");
        assert_eq!(payout_script_hex(VALID_1).unwrap(), hex1, "deterministic");
        assert_ne!(payout_script_hex(VALID_2).unwrap(), hex1);
    }
    #[test]
    fn payout_script_rejects_non_taproot() {
        // Encode a valid-checksum witness-v0 din address; only v1/32-byte
        // (Taproot) can receive SV2 pool payouts.
        let hrp = bech32::Hrp::parse("din").unwrap();
        let v0 = bech32::segwit::encode(hrp, bech32::Fe32::Q, &[0u8; 20]).unwrap();
        assert_eq!(payout_script_hex(&v0), Err(AddressError::NotTaproot));
    }
    #[test]
    fn payout_script_rejects_invalid_input() {
        assert_eq!(payout_script_hex("tdin1qqqqqq"), Err(AddressError::WrongNetwork));
        assert_eq!(payout_script_hex("garbage"), Err(AddressError::Invalid));
    }
    #[test]
    fn rejects_garbage() {
        assert_eq!(validate_address("hello world"), Err(AddressError::Invalid));
        assert_eq!(
            validate_address("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"),
            Err(AddressError::Invalid)
        );
    }
}
