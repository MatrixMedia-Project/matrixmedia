//! Integration test: mm-dns starts and serves `GET /healthz` -> 200 `{"ok":true}`.
//!
//! Follows the same pattern as `crates/mm-fakestripe/tests/roundtrip.rs`:
//! spawn the compiled binary on an ephemeral port, poll `/healthz` until it
//! responds, then assert on the response.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread")]
async fn healthz_returns_200_with_ok_true() {
    // Find an unused port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    let bind_addr = format!("127.0.0.1:{port}");

    let bin = env!("CARGO_BIN_EXE_mm-dns");
    let mut child = Command::new(bin)
        .env("MM_DNS_BIND", &bind_addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mm-dns");

    let http = reqwest::Client::new();
    let url = format!("http://{bind_addr}/healthz");
    let deadline = Instant::now() + Duration::from_secs(5);
    let resp = loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("mm-dns did not start within 5s");
        }
        match http.get(&url).send().await {
            Ok(r) if r.status().is_success() => break r,
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("parse healthz JSON body");
    assert_eq!(body, serde_json::json!({"ok": true}));

    let _ = child.kill();
    let _ = child.wait();
}
