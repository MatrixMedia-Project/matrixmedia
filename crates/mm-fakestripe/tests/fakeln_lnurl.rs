//! Integration test: drive `mm-payment::lnurl::LnurlPayClient` against the
//! fakeln endpoints exposed by the running mm-fakestripe binary.
//!
//! Verifies the full LUD-16 → LUD-06 → invoice round trip works without a
//! real Lightning node, which is the demo path mm-server uses when a
//! creator publishes `<name>@localhost:<port>` as their Lightning Address.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mm_payment::lnurl::LnurlPayClient;

#[tokio::test(flavor = "multi_thread")]
async fn lnurl_pay_client_roundtrips_through_fakeln() {
    // Random port — let the OS assign one.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let listen_addr = format!("127.0.0.1:{port}");

    let bin = env!("CARGO_BIN_EXE_mm-fakestripe");
    let mut child = Command::new(bin)
        .env("MM_FAKESTRIPE_LISTEN", &listen_addr)
        .env("MM_FAKESTRIPE_WEBHOOK_URL", "http://127.0.0.1:1/nowhere")
        .env("MM_FAKESTRIPE_WEBHOOK_SECRET", "whsec_irrelevant")
        .env("MM_FAKESTRIPE_WEBHOOK_DELAY_MS", "10")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mm-fakestripe");

    // Wait for /healthz.
    let http = reqwest::Client::new();
    let base = format!("http://{listen_addr}");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("mm-fakestripe did not start within 5s");
        }
        if http
            .get(format!("{base}/healthz"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Drive the real client.
    let address = format!("alice@127.0.0.1:{port}");
    let client = LnurlPayClient::new();

    let invoice = client
        .request_invoice(&address, 5_000, Some("hello from a unit test"))
        .await
        .unwrap_or_else(|e| {
            let _ = child.kill();
            panic!("LnurlPayClient round trip failed: {e}");
        });

    assert!(
        invoice.pr.starts_with("lnbc"),
        "fake BOLT11 should start with `lnbc`, got: {}",
        invoice.pr
    );
    assert!(
        invoice.success_action.is_some(),
        "fakeln should populate a successAction"
    );

    // Bounds enforcement is exercised in unit tests; here we just confirm the
    // happy path. Tear down.
    let _ = child.kill();
    let _ = child.wait();
}
