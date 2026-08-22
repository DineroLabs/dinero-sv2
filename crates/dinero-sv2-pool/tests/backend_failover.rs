use dinero_sv2_pool::backend::BackendPool;
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
                        write_response(&mut stream, Value::Null, Value::Null).await;
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

fn state(work: &str, hash: &str, ready: bool) -> FakeState {
    FakeState {
        chainwork: work.into(),
        best_hash: hash.into(),
        template_ready: ready,
        record_submitted_block: true,
        submitted_header_known: false,
        submit_count: 0,
        submit_delay: Duration::ZERO,
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
