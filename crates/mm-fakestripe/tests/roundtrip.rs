//! Round-trip tests: verify the JSON returned by mm-fakestripe actually
//! deserializes into the same async-stripe types that mm-core expects.
//!
//! This catches drift between the fake and async-stripe's schema without
//! needing to boot mm-core.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Spawn the mm-fakestripe binary on a random port, wait until /healthz responds,
/// exercise the endpoints mm-core hits, and verify each response deserializes
/// cleanly into the corresponding `stripe` type.
#[tokio::test(flavor = "multi_thread")]
async fn fake_responses_deserialize_into_async_stripe_types() {
    // Find an unused port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let listen_addr = format!("127.0.0.1:{port}");
    let webhook_url = "http://127.0.0.1:1/nowhere".to_string(); // webhook will fail, that's fine
    let secret = "whsec_testtesttesttesttesttesttest".to_string();

    // Build the binary (debug) and launch it.
    let bin = env!("CARGO_BIN_EXE_mm-fakestripe");
    let mut child = Command::new(bin)
        .env("MM_FAKESTRIPE_LISTEN", &listen_addr)
        .env("MM_FAKESTRIPE_WEBHOOK_URL", &webhook_url)
        .env("MM_FAKESTRIPE_WEBHOOK_SECRET", &secret)
        .env("MM_FAKESTRIPE_WEBHOOK_DELAY_MS", "50")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mm-fakestripe");

    // Wait up to 5s for /healthz.
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

    // --- 1. POST /v1/accounts ---
    let resp = http
        .post(format!("{base}/v1/accounts"))
        .form(&[("type", "express"), ("metadata[mm_user_id]", "@u:ex.com")])
        .send()
        .await
        .expect("POST /v1/accounts");
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    let account: stripe::Account = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!("Account JSON did not deserialize: {e}\nbody: {body}");
    });
    assert!(account.charges_enabled.unwrap_or(false));
    assert_eq!(account.type_, Some(stripe::AccountType::Express));

    // --- 2. GET /v1/accounts/{id} ---
    let acct_id = account.id.as_str();
    let resp = http
        .get(format!("{base}/v1/accounts/{acct_id}"))
        .send()
        .await
        .expect("GET /v1/accounts/{id}");
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    let retrieved: stripe::Account = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!("Retrieved Account JSON did not deserialize: {e}\nbody: {body}");
    });
    assert!(retrieved.charges_enabled.unwrap_or(false));

    // --- 3. POST /v1/account_links ---
    let resp = http
        .post(format!("{base}/v1/account_links"))
        .form(&[
            ("account", acct_id),
            ("type", "account_onboarding"),
            ("return_url", "http://ret"),
            ("refresh_url", "http://ref"),
        ])
        .send()
        .await
        .expect("POST /v1/account_links");
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    let link: stripe::AccountLink = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!("AccountLink JSON did not deserialize: {e}\nbody: {body}");
    });
    assert!(!link.url.is_empty());

    // --- 4. POST /v1/checkout/sessions ---
    let resp = http
        .post(format!("{base}/v1/checkout/sessions"))
        .form(&[
            ("mode", "payment"),
            ("success_url", "http://ok"),
            ("cancel_url", "http://no"),
            ("metadata[donation_id]", "d_test"),
            ("line_items[0][price_data][currency]", "usd"),
            ("line_items[0][price_data][unit_amount]", "500"),
            ("line_items[0][quantity]", "1"),
        ])
        .send()
        .await
        .expect("POST /v1/checkout/sessions");
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    let session: stripe::CheckoutSession =
        serde_json::from_str(&body).unwrap_or_else(|e| {
            panic!("CheckoutSession JSON did not deserialize: {e}\nbody: {body}");
        });
    assert_eq!(session.mode, stripe::CheckoutSessionMode::Payment);
    assert!(session.url.is_some());
    assert!(session.id.as_str().starts_with("cs_fake"));

    // Teardown.
    let _ = child.kill();
    let _ = child.wait();
}
