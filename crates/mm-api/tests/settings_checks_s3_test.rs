//! S3 probe against a real S3 server. Needs `--features s3` and MM_TEST_S3_ENDPOINT /
//! _BUCKET / _ACCESS_KEY / _SECRET_KEY (local: `just dev-s3`, SeaweedFS in
//! infra/docker). Skips loudly when unset.
#![cfg(feature = "s3")]

use mm_api::settings_checks::{Check, run};
use mm_core::config::Config;

#[tokio::test]
async fn s3_probe_round_trips_a_real_bucket() {
    let Ok(endpoint) = std::env::var("MM_TEST_S3_ENDPOINT") else {
        eprintln!("SKIPPED: MM_TEST_S3_ENDPOINT unset");
        return;
    };
    let mut cfg = Config::default();
    cfg.storage.s3.endpoint = Some(endpoint);
    cfg.storage.s3.bucket = std::env::var("MM_TEST_S3_BUCKET").unwrap();
    cfg.storage.s3.access_key = std::env::var("MM_TEST_S3_ACCESS_KEY").unwrap();
    cfg.storage.s3.secret_key = std::env::var("MM_TEST_S3_SECRET_KEY").unwrap();
    cfg.storage.s3.path_style = true;
    let r = run(Check::S3, &serde_json::Map::new(), &cfg).await;
    assert!(r.ok, "{r:?}");
}
