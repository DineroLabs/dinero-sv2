use dinero_sv2_pool::backend::{BackendPool, BlockSubmission};
use dinero_sv2_pool::rpc::{Auth, RpcClient, SubmitBlockResult};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone)]
struct FakeState {
    chainwork: String,
    best_hash: String,
    template_ready: bool,
    record_submitted_block: bool,
    submitted_header_known: bool,
    submit_count: u64,
    submit_delay: Duration,
    reject_submissions: bool,
}

async fn read_request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n > 0, "unexpected EOF reading fake RPC request");
        bytes.extend_from_slice(&buf[..n]);
        if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length: usize = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::trim)
                .and_then(|v| v.parse().ok())
        })
        .unwrap();
    while bytes.len() < header_end + content_length {
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n > 0, "unexpected EOF reading fake RPC body");
        bytes.extend_from_slice(&buf[..n]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap()
}

async fn write_response(stream: &mut TcpStream, result: Value, error: Value) {
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "1.0",
        "id": 0,
        "result": result,
        "error": error,
    }))
    .unwrap();
    let header = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await.unwrap();
    stream.write_all(&body).await.unwrap();
}

async fn spawn_backend(
    initial: FakeState,
    timeout: Duration,
) -> (Arc<Mutex<FakeState>>, Arc<RpcClient>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = Arc::new(Mutex::new(initial));
    let server_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let state = Arc::clone(&server_state);
            tokio::spawn(async move {
                let request = read_request(&mut stream).await;
                let method = request["method"].as_str().unwrap();
                let snapshot = state.lock().unwrap().clone();
                match method {
                    "getblockchaininfo" => {
                        write_response(
                            &mut stream,
                            json!({
                                "blocks": 100,
                                "headers": 100,
                                "bestblockhash": snapshot.best_hash,
                                "chainwork": snapshot.chainwork,
                                "initialblockdownload": false,
                            }),
                            Value::Null,
                        )
                        .await;
                    }
                    "getblocktemplate" if snapshot.template_ready => {
                        write_response(&mut stream, json!({"height": 101}), Value::Null).await;
                    }
                    "getblocktemplate" => {
                        write_response(
                            &mut stream,
                            json!({"error": "header_chain_mismatch"}),
                            Value::Null,
                        )
                        .await;
                    }
                    "submitblock" => {
                        {
                            let mut state = state.lock().unwrap();
                            state.submitted_header_known = state.record_submitted_block;
                            state.submit_count += 1;
                        }
                        tokio::time::sleep(snapshot.submit_delay).await;
                        let result = if snapshot.reject_submissions {
                            json!("bad-prevblk")
                        } else {
                            Value::Null
                        };
                        write_response(&mut stream, result, Value::Null).await;
                    }
                    "getblock" => {
                        let requested_hash = request["params"][0].as_str().unwrap();
                        if snapshot.submitted_header_known {
                            write_response(
                                &mut stream,
                                json!({"hash": requested_hash, "height": 101}),
                                Value::Null,
                            )
                            .await;
                        } else {
                            write_response(
                                &mut stream,
                                json!({"error": "Block not found"}),
                                Value::Null,
                            )
                            .await;
                        }
                    }
                    other => panic!("unexpected fake RPC method {other}"),
                }
            });
        }
    });
    let client = Arc::new(
        RpcClient::with_timeout(
            format!("http://{address}"),
            Auth::UserPass("user".into(), "pass".into()),
            timeout,
        )
        .unwrap(),
    );
    (state, client)
}

/// `work` is bare hex; the fake serves it 0x-PREFIXED because that is what a
/// real dinerod's getblockchaininfo returns. Serving bare hex here is what let
/// a parser that rejected the prefix ship green — the pool then treated every
/// live backend as unusable and produced no templates at all.
fn state(work: &str, hash: &str, ready: bool) -> FakeState {
    FakeState {
        chainwork: format!("0x{work}"),
        best_hash: hash.into(),
        template_ready: ready,
        record_submitted_block: true,
        submitted_header_known: false,
        submit_count: 0,
        submit_delay: Duration::ZERO,
        reject_submissions: false,
    }
}

#[tokio::test]
async fn greatest_work_backend_fails_over_and_recovers_without_a_vote() {
    let (high_state, high) =
        spawn_backend(state("200", "high", true), Duration::from_secs(1)).await;
    let (_low_state, low) = spawn_backend(state("100", "low", true), Duration::from_secs(1)).await;
    let pool = BackendPool::new(vec![high, low]).unwrap();

    let (first, _) = pool.select_template("rdin-test").await.unwrap();
    assert_eq!(first.health.best_hash, "high");

    high_state.lock().unwrap().template_ready = false;
    let (fallback, _) = pool.select_template("rdin-test").await.unwrap();
    assert_eq!(fallback.health.best_hash, "low");
    assert!(fallback.switched);
    assert!(fallback.epoch > first.epoch);

    high_state.lock().unwrap().template_ready = true;
    let (recovered, _) = pool.select_template("rdin-test").await.unwrap();
    assert_eq!(recovered.health.best_hash, "high");
    assert!(recovered.epoch > fallback.epoch);
}

#[tokio::test]
async fn submit_timeout_is_reconciled_as_accepted_from_full_block_presence() {
    let mut delayed = state("100", "tip", true);
    delayed.submit_delay = Duration::from_millis(200);
    let (_state, client) = spawn_backend(delayed, Duration::from_millis(50)).await;
    let pool = BackendPool::new(vec![client]).unwrap();
    let block = "00".repeat(128);
    assert_eq!(
        pool.submit_block(&block).await.unwrap(),
        SubmitBlockResult::Accepted
    );
}

#[tokio::test]
async fn submit_timeout_without_a_readable_block_remains_unknown() {
    let mut delayed = state("100", "tip", true);
    delayed.submit_delay = Duration::from_millis(200);
    delayed.record_submitted_block = false;
    let (_state, client) = spawn_backend(delayed, Duration::from_millis(50)).await;
    let pool = BackendPool::new(vec![client]).unwrap();

    assert!(pool.submit_block(&"00".repeat(128)).await.is_err());
}

#[tokio::test]
async fn accepted_block_is_fanned_out_to_every_backend() {
    let (first_state, first) =
        spawn_backend(state("100", "tip", true), Duration::from_secs(1)).await;
    let (second_state, second) =
        spawn_backend(state("100", "tip", true), Duration::from_secs(1)).await;
    let pool = BackendPool::new(vec![first, second]).unwrap();

    assert_eq!(
        pool.submit_block(&"00".repeat(128)).await.unwrap(),
        SubmitBlockResult::Accepted
    );
    assert_eq!(first_state.lock().unwrap().submit_count, 1);
    assert_eq!(second_state.lock().unwrap().submit_count, 1);
}

/// A 128-byte header-only block whose prev-hash field (bytes 4..36) is
/// filled with `parent` and whose last byte is `nonce`, so two blocks can
/// share a parent and still differ.
fn block_on(parent: u8, nonce: u8) -> String {
    let mut bytes = [0u8; 128];
    bytes[4..36].fill(parent);
    bytes[127] = nonce;
    hex::encode(bytes)
}

// The pool floods its node when every block-worthy share on one parent is
// submitted: the first one wins, the rest can only be orphans or rejects.

#[tokio::test]
async fn candidates_on_an_already_accepted_parent_are_not_submitted() {
    let (backend, client) = spawn_backend(state("100", "tip", true), Duration::from_secs(1)).await;
    let pool = BackendPool::new(vec![client]).unwrap();

    assert_eq!(
        pool.submit_found_block(&block_on(1, 1)).await.unwrap(),
        BlockSubmission::Submitted(SubmitBlockResult::Accepted)
    );
    assert!(matches!(
        pool.submit_found_block(&block_on(1, 2)).await.unwrap(),
        BlockSubmission::Skipped(_)
    ));
    assert_eq!(backend.lock().unwrap().submit_count, 1);

    // A new parent is a new race: submitted normally.
    assert_eq!(
        pool.submit_found_block(&block_on(2, 1)).await.unwrap(),
        BlockSubmission::Submitted(SubmitBlockResult::Accepted)
    );
    assert_eq!(backend.lock().unwrap().submit_count, 2);
}

#[tokio::test]
async fn a_second_candidate_on_a_parent_already_in_flight_is_not_submitted() {
    let mut slow = state("100", "tip", true);
    slow.submit_delay = Duration::from_millis(300);
    let (backend, client) = spawn_backend(slow, Duration::from_secs(2)).await;
    let pool = BackendPool::new(vec![client]).unwrap();

    let first_block = block_on(1, 1);
    let second_block = block_on(1, 2);
    let (first, second) = tokio::join!(pool.submit_found_block(&first_block), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        pool.submit_found_block(&second_block).await
    });
    assert_eq!(
        first.unwrap(),
        BlockSubmission::Submitted(SubmitBlockResult::Accepted)
    );
    assert!(matches!(second.unwrap(), BlockSubmission::Skipped(_)));
    assert_eq!(backend.lock().unwrap().submit_count, 1);
}

#[tokio::test]
async fn a_submission_in_flight_does_not_hold_up_a_different_parent() {
    let mut slow = state("100", "tip", true);
    slow.submit_delay = Duration::from_millis(300);
    let (backend, client) = spawn_backend(slow, Duration::from_secs(2)).await;
    let pool = BackendPool::new(vec![client]).unwrap();

    let old_parent = block_on(1, 1);
    let new_parent = block_on(2, 1);
    let (first, second) = tokio::join!(pool.submit_found_block(&old_parent), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        pool.submit_found_block(&new_parent).await
    });
    assert_eq!(
        first.unwrap(),
        BlockSubmission::Submitted(SubmitBlockResult::Accepted)
    );
    assert_eq!(
        second.unwrap(),
        BlockSubmission::Submitted(SubmitBlockResult::Accepted)
    );
    assert_eq!(backend.lock().unwrap().submit_count, 2);
}

#[tokio::test]
async fn a_rejected_candidate_lets_the_next_one_on_that_parent_try() {
    let mut rejecting = state("100", "tip", true);
    rejecting.reject_submissions = true;
    let (backend, client) = spawn_backend(rejecting, Duration::from_secs(1)).await;
    let pool = BackendPool::new(vec![client]).unwrap();

    for nonce in 1..=2 {
        assert_eq!(
            pool.submit_found_block(&block_on(1, nonce)).await.unwrap(),
            BlockSubmission::Submitted(SubmitBlockResult::Rejected("bad-prevblk".into()))
        );
    }
    assert_eq!(backend.lock().unwrap().submit_count, 2);
}

#[tokio::test]
async fn a_failed_submission_lets_the_next_one_on_that_parent_try() {
    let mut lost = state("100", "tip", true);
    lost.submit_delay = Duration::from_millis(200);
    lost.record_submitted_block = false;
    let (backend, client) = spawn_backend(lost, Duration::from_millis(50)).await;
    let pool = BackendPool::new(vec![client]).unwrap();

    assert!(pool.submit_found_block(&block_on(1, 1)).await.is_err());
    assert!(pool.submit_found_block(&block_on(1, 2)).await.is_err());
    assert_eq!(backend.lock().unwrap().submit_count, 2);
}

#[tokio::test]
async fn a_parent_reselected_after_reorg_can_be_mined_again() {
    let (backend, client) =
        spawn_backend(state("100", &"01".repeat(32), true), Duration::from_secs(1)).await;
    let pool = BackendPool::new(vec![client]).unwrap();
    pool.select_template("fixture").await.unwrap();
    assert_eq!(
        pool.submit_found_block(&block_on(1, 1)).await.unwrap(),
        BlockSubmission::Submitted(SubmitBlockResult::Accepted)
    );
    assert!(matches!(
        pool.submit_found_block(&block_on(1, 2)).await.unwrap(),
        BlockSubmission::Skipped(_)
    ));
    backend.lock().unwrap().best_hash = "02".repeat(32);
    pool.select_template("fixture").await.unwrap();
    // A normal chain selection, not a new pool process, returns to the parent.
    backend.lock().unwrap().best_hash = "01".repeat(32);
    pool.select_template("fixture").await.unwrap();
    assert_eq!(
        pool.submit_found_block(&block_on(1, 3)).await.unwrap(),
        BlockSubmission::Submitted(SubmitBlockResult::Accepted)
    );
    assert_eq!(backend.lock().unwrap().submit_count, 2);
}
