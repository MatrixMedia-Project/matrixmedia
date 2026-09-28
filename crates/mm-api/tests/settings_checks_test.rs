//! "Test connection" probes through the public `settings_checks::run`, against real
//! sockets. No database needed.

use std::time::Duration;

use mm_api::settings_checks::{Check, run};
use mm_core::config::Config;
use serde_json::{Map, Value, json};

/// A listener that accepts every connection and never reads or answers: the failure mode
/// of a firewalled or wedged service that a probe must not wait on forever.
async fn silent_host() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    format!("http://{addr}")
}

/// Every HTTP probe gives up on its own (5 s) instead of hanging the dashboard's request.
/// The probes run concurrently; the 20 s bound only catches a probe with no timeout.
#[tokio::test]
async fn http_probes_time_out_when_the_host_never_answers() {
    let url = silent_host().await;
    let mut saved = Config::default();
    // Read-only destinations and the Stripe key come from the saved config; LNbits from the form.
    saved.matrix.homeserver_url = url.clone();
    saved.sfu.livekit_url = Some(url.clone());
    saved.monetization.stripe_api_base = url.clone();
    saved.monetization.stripe_secret_key = "sk_test_x".into();
    let lnbits: Map<String, Value> = [
        ("monetization.lnbits_url".to_string(), json!(url)),
        ("monetization.lnbits_invoice_key".to_string(), json!("inv")),
    ]
    .into_iter()
    .collect();
    let none = Map::new();

    let all = async {
        tokio::join!(
            run(Check::Homeserver, &none, &saved),
            run(Check::Livekit, &none, &saved),
            run(Check::Stripe, &none, &saved),
            run(Check::Lnbits, &lnbits, &saved),
        )
    };
    let (homeserver, livekit, stripe, lnbits) = tokio::time::timeout(Duration::from_secs(20), all)
        .await
        .expect("every probe must bound its own request instead of hanging");
    for (name, r) in [
        ("homeserver", homeserver),
        ("livekit", livekit),
        ("stripe", stripe),
        ("lnbits", lnbits),
    ] {
        assert!(!r.ok && r.detail.contains("timed out"), "{name}: {r:?}");
    }
}
