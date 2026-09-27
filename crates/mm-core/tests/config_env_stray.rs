//! Integration test for the "stray env reads" fold-in (Task 2 of the
//! dashboard-configuration plan).
//!
//! This test mutates process-wide env vars, so it must NOT live in
//! `mm-core`'s lib test module alongside the other env-mutating tests —
//! those run concurrently (in-process, multi-threaded) and would race this
//! one. Living in its own integration test binary gives it a separate
//! process, per Ruling R2 of task-2-brief.md.

use mm_core::config::Config;

#[test]
fn stray_env_reads_now_live_in_config() {
    let vars = [
        "MM_TURN_URLS", "MM_TURN_TTL_SECS", "MM_TURN_SHARED_SECRET",
        "MM_SFU_LIVEKIT_PUBLIC_URL", "MM_FEED_ENABLED", "MM_SERVER_REQUEST_WEBHOOK_URL",
        "MM_SWITCH_AUTH_SECRET", "MM_SWITCH_LEGACY_LK_SOURCE", "MM_WIDGET_DIR",
    ];
    let saved: Vec<_> = vars.iter().map(|v| (*v, std::env::var(v).ok())).collect();
    let set = |k: &str, v: &str| unsafe { std::env::set_var(k, v) };

    // Defaults when nothing is set.
    for v in vars { unsafe { std::env::remove_var(v) } }
    let mut c = Config::default();
    c.apply_env_overrides();
    assert!(c.turn.urls.is_empty());
    assert_eq!(c.turn.ttl_secs, 86_400);
    assert_eq!(c.turn.shared_secret_opt(), None);
    assert_eq!(c.sfu.livekit_public_url, None);
    assert!(c.server.feed_enabled, "feed defaults to on");
    assert_eq!(c.server.request_webhook_url, None);
    assert_eq!(c.advertising.switch_auth_secret_opt(), None);
    assert!(c.advertising.switch_legacy_lk_source, "legacy LK source defaults to on");
    assert_eq!(c.server.widget_dir, None);

    // Values, with the old parsing rules.
    set("MM_TURN_URLS", "turn:a.example:3478, ,turns:b.example:5349");
    set("MM_TURN_TTL_SECS", "3600");
    set("MM_TURN_SHARED_SECRET", "mm-test-secret-7f3a");
    set("MM_SFU_LIVEKIT_PUBLIC_URL", "wss://matrix.example/livekit");
    set("MM_FEED_ENABLED", "Off");
    set("MM_SERVER_REQUEST_WEBHOOK_URL", "https://hooks.example/abc");
    set("MM_SWITCH_AUTH_SECRET", "switch-secret");
    set("MM_SWITCH_LEGACY_LK_SOURCE", "0");
    set("MM_WIDGET_DIR", "/srv/widget");
    let mut c = Config::default();
    c.apply_env_overrides();
    assert_eq!(c.turn.urls, vec!["turn:a.example:3478", "turns:b.example:5349"]);
    assert_eq!(c.turn.ttl_secs, 3600);
    assert_eq!(c.turn.shared_secret_opt(), Some("mm-test-secret-7f3a"));
    assert_eq!(c.sfu.livekit_public_url.as_deref(), Some("wss://matrix.example/livekit"));
    assert!(!c.server.feed_enabled);
    assert_eq!(c.server.request_webhook_url.as_deref(), Some("https://hooks.example/abc"));
    assert_eq!(c.advertising.switch_auth_secret_opt(), Some("switch-secret"));
    assert!(!c.advertising.switch_legacy_lk_source);
    assert_eq!(c.server.widget_dir.as_deref(), Some("/srv/widget"));

    // Empty strings mean "unset" (compose passes `${VAR:-}`); a zero TTL is ignored.
    set("MM_TURN_TTL_SECS", "0");
    set("MM_SERVER_REQUEST_WEBHOOK_URL", "");
    set("MM_SFU_LIVEKIT_PUBLIC_URL", "");
    set("MM_TURN_SHARED_SECRET", "");
    set("MM_WIDGET_DIR", "");
    let mut c = Config::default();
    c.apply_env_overrides();
    assert_eq!(c.turn.ttl_secs, 86_400);
    assert_eq!(c.server.request_webhook_url, None);
    assert_eq!(c.sfu.livekit_public_url, None);
    assert_eq!(c.turn.shared_secret_opt(), None);
    assert_eq!(c.server.widget_dir, None, "MM_WIDGET_DIR=\"\" must mean not served");

    for (k, v) in saved {
        match v { Some(v) => set(k, &v), None => unsafe { std::env::remove_var(k) } }
    }
}
