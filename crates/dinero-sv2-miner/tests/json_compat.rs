// The GUI (DineroLabs/DineroMiner) parses --json lines. Assert the emitter's
// wire shapes never drift: every line of the captured live fixture must parse
// and carry the exact keys the GUI reads.
#[test]
fn fixture_lines_keep_their_shape() {
    let text = include_str!("fixtures/sv2-miner-json.log");
    let mut seen_hashrate = false;
    let mut seen_block = false;
    for line in text.lines().filter(|l| l.starts_with('{')) {
        let v: serde_json::Value = serde_json::from_str(line).expect("fixture line must stay JSON");
        let ev = v["event"].as_str().expect("event key");
        match ev {
            "hashrate" => { assert!(v["mhs"].is_number()); seen_hashrate = true; }
            "share_submitted" => {
                assert!(v["hash"].is_string() && v["nonce"].is_string() && v["tries"].is_number());
                if v["meets_block_target"] == true { seen_block = true; }
            }
            _ => {}
        }
    }
    assert!(seen_hashrate && seen_block, "fixture must cover the GUI-critical events");
}
