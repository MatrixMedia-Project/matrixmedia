use chrono::{Duration, TimeZone, Utc};
use mm_core::fleet::NodeId;
use mm_fleet::test_boot::*;

#[test]
fn a_request_and_its_node_name_each_other() {
    let node = node_id_for("r-0123abcd");
    assert_eq!(node, NodeId::new("tb-0123abcd"));
    assert_eq!(request_id_for(node.as_str()).as_deref(), Some("r-0123abcd"));
    assert_eq!(request_id_for("bc-b1-transcode-0"), None);
}

#[test]
fn the_report_url_is_https_and_ends_in_the_report_path() {
    assert_eq!(
        report_url("https://matrix.example.org/").unwrap(),
        "https://matrix.example.org/_mm/webhooks/fleet/boot-report"
    );
    assert!(
        report_url("http://matrix.example.org").is_err(),
        "a token never crosses the internet in clear"
    );
    assert!(report_url("https://matrix.example.org/a b").is_err());
    assert!(report_url("not a url").is_err());
}

#[test]
fn tokens_are_256_bit_hex_and_only_their_hash_is_kept() {
    let (a, ha) = mint_token();
    let (b, _) = mint_token();
    assert_ne!(a, b);
    assert!(looks_like_token(&a));
    assert_eq!(ha, token_hash(&a));
    assert_eq!(ha.len(), 32);
    assert!(!looks_like_token("Z".repeat(64).as_str()));
    assert!(!looks_like_token(&a[..63]));
}

#[test]
fn every_minted_token_passes_the_shape_check_and_uppercase_does_not() {
    for _ in 0..200 {
        let (token, _) = mint_token();
        assert!(looks_like_token(&token), "{token}");
    }
    assert!(
        !looks_like_token(&"A".repeat(64)),
        "uppercase hex is not what mint_token makes"
    );
}

#[test]
fn the_probe_cloud_init_carries_the_url_and_token_in_a_0600_file_and_runs_once() {
    let ci = probe_cloud_init(
        "https://matrix.example.org/_mm/webhooks/fleet/boot-report",
        &"a".repeat(64),
    );
    assert!(ci.starts_with("#cloud-config\n"), "{ci}");
    assert!(ci.contains(
        "      MM_REPORT_URL=https://matrix.example.org/_mm/webhooks/fleet/boot-report\n"
    ));
    assert!(ci.contains(&format!("      MM_REPORT_TOKEN={}\n", "a".repeat(64))));
    assert!(ci.contains("permissions: \"0600\""));
    assert!(ci.contains("h264_nvenc"), "the probe encodes with NVENC");
    assert!(ci.contains("runcmd:\n  - [ /usr/local/sbin/mm-boot-probe ]"));
    // Every script line sits inside the YAML block (6 spaces) or is empty.
    let script = ci
        .split("    content: |\n")
        .nth(2)
        .unwrap()
        .split("runcmd:")
        .next()
        .unwrap();
    assert!(
        script
            .lines()
            .all(|l| l.is_empty() || l.starts_with("      ")),
        "{script}"
    );
}

#[test]
fn a_boot_report_has_a_strict_shape() {
    let ok: BootReport = serde_json::from_str(
        r#"{"v":1,"gpu":"NVIDIA L4, 550.90","nvenc":"ok","nvenc_error":null,"uptime_secs":95,"probe_secs":70}"#,
    )
    .unwrap();
    assert!(ok.validate().is_ok());
    assert!(
        serde_json::from_str::<BootReport>(
            r#"{"v":1,"gpu":"x","nvenc":"ok","uptime_secs":1,"probe_secs":1,"extra":1}"#
        )
        .is_err()
    );
    let mut bad = ok.clone();
    bad.nvenc = "maybe".into();
    assert!(bad.validate().is_err());
    let mut long = ok.clone();
    long.gpu = "x".repeat(201);
    assert!(long.validate().is_err());
    let at = Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap();
    assert_eq!(report_of(&stored_report(&ok, at)), Some(ok));
}

#[test]
fn billing_rounds_up_to_whole_minutes_and_costs_round_up_to_cents() {
    let t0 = Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap();
    assert_eq!(billed_minutes(t0, t0), 1);
    assert_eq!(billed_minutes(t0, t0 + Duration::seconds(61)), 2);
    assert_eq!(billed_minutes(t0, t0 + Duration::minutes(12)), 12);
    assert_eq!(billed_minutes(t0, t0 + Duration::seconds(60)), 1);
    assert_eq!(billed_minutes(t0, t0 + Duration::milliseconds(60_500)), 2);
    assert_eq!(estimate_cost(Some(0.79), 12), Some(0.16));
    assert_eq!(estimate_cost(Some(1.12), 15), Some(0.28));
    assert_eq!(estimate_cost(None, 12), None);
}

#[test]
fn a_transcoder_cloud_init_names_its_node_and_nothing_else() {
    let ci = transcode_cloud_init(&NodeId::new("bc-b1-transcode-0"));
    assert!(ci.contains("MM_NODE_ID=bc-b1-transcode-0"));
    // Only alphanumerics, '-', '_' and '.' survive: the newline, spaces, colon, brackets and slash
    // are dropped, so the id stays one scalar on its own line.
    let odd = transcode_cloud_init(&NodeId::new("bc-x\nruncmd: [rm -rf /]-transcode-0"));
    assert_eq!(
        odd,
        "#cloud-config\nwrite_files:\n  - path: /etc/mm-transcode.env\n    permissions: \"0600\"\n    content: |\n      MM_NODE_ID=bc-xruncmdrm-rf-transcode-0\n      MM_NODE_FLAVOR=transcode\n"
    );
}

#[test]
fn a_bare_tb_prefix_is_no_request() {
    assert_eq!(request_id_for("tb-"), None);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "starts with r-")]
fn node_id_for_refuses_a_request_id_without_the_r_prefix() {
    node_id_for("0123abcd");
}

#[test]
fn report_url_refuses_a_url_that_could_hide_the_report_path() {
    for (public_url, problem) in [
        ("https://matrix.example.org/?x=1", "query"),
        ("https://matrix.example.org/?", "query"),
        ("https://matrix.example.org/#top", "fragment"),
        ("https://matrix.example.org/#", "fragment"),
        ("https://user:pass@matrix.example.org", "userinfo"),
        ("https://:pass@matrix.example.org", "userinfo"),
        ("https://user@matrix.example.org", "userinfo"),
    ] {
        let err = report_url(public_url).unwrap_err();
        assert!(err.contains(problem), "{public_url}: {err}");
    }
}

#[test]
fn report_url_accepts_an_origin_with_a_port_or_a_path_prefix() {
    assert_eq!(
        report_url("https://matrix.example.org:8443").unwrap(),
        "https://matrix.example.org:8443/_mm/webhooks/fleet/boot-report"
    );
    assert_eq!(
        report_url("https://matrix.example.org/prefix/").unwrap(),
        "https://matrix.example.org/prefix/_mm/webhooks/fleet/boot-report"
    );
}

#[test]
fn a_price_that_is_not_a_rate_has_no_estimate() {
    assert_eq!(estimate_cost(Some(f64::NAN), 15), None);
    assert_eq!(estimate_cost(Some(f64::INFINITY), 15), None);
    assert_eq!(estimate_cost(Some(-1.0), 15), None);
    assert_eq!(estimate_cost(Some(0.0), 15), Some(0.0));
}

#[test]
fn a_backwards_clock_bills_one_minute() {
    let t0 = Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap();
    assert_eq!(billed_minutes(t0 + Duration::seconds(5), t0), 1);
}

fn sample_report() -> BootReport {
    BootReport {
        v: 1,
        gpu: "NVIDIA L4, 550.90".into(),
        nvenc: "ok".into(),
        nvenc_error: None,
        uptime_secs: 95,
        probe_secs: 70,
    }
}

#[test]
fn a_stored_report_that_fails_validation_is_absent() {
    let stored = serde_json::json!({
        "report": {"v": 1, "gpu": "x".repeat(201), "nvenc": "ok", "nvenc_error": null,
                   "uptime_secs": 1, "probe_secs": 1},
        "received_at": "2026-10-07T12:00:00Z"
    });
    assert_eq!(report_of(&stored), None);
}

#[test]
fn validate_refuses_a_wrong_version() {
    let mut r = sample_report();
    r.v = 2;
    assert_eq!(r.validate(), Err("v must be 1"));
}

#[test]
fn validate_bounds_nvenc_error_to_400_characters() {
    let mut r = sample_report();
    r.nvenc = "fail".into();
    r.nvenc_error = Some("e".repeat(400));
    assert!(r.validate().is_ok());
    r.nvenc_error = Some("e".repeat(401));
    assert_eq!(r.validate(), Err("nvenc_error is at most 400 characters"));
}

#[test]
fn validate_bounds_uptime_and_probe_time_to_a_day() {
    let mut r = sample_report();
    r.uptime_secs = 86_400;
    r.probe_secs = 86_400;
    assert!(r.validate().is_ok());
    r.uptime_secs = 86_401;
    assert_eq!(r.validate(), Err("uptime_secs is at most 86400"));
    r.uptime_secs = 95;
    r.probe_secs = 86_401;
    assert_eq!(r.validate(), Err("probe_secs is at most 86400"));
}

/// Postgres refuses a NUL in JSONB, so a report holding one must be refused as malformed (400),
/// not fail as a server error on every retry until the boot ends as "no report".
#[test]
fn validate_refuses_control_characters_but_keeps_newlines_and_tabs() {
    let mut r = sample_report();
    r.gpu = "NVIDIA L4\n\t550.90".into();
    assert!(r.validate().is_ok());
    for bad in [
        "NVIDIA\0L4",
        "NVIDIA\u{1b}[31mL4",
        "NVIDIA\rL4",
        "NVIDIA\u{7f}",
        "NVIDIA\u{85}",
    ] {
        r.gpu = bad.into();
        assert_eq!(
            r.validate(),
            Err("gpu must not hold control characters"),
            "{bad:?}"
        );
    }

    let mut r = sample_report();
    r.nvenc = "fail".into();
    r.nvenc_error = Some("OpenEncodeSessionEx failed\n\tcode 8".into());
    assert!(r.validate().is_ok());
    for bad in ["no\0device", "no\u{7}device"] {
        r.nvenc_error = Some(bad.into());
        assert_eq!(
            r.validate(),
            Err("nvenc_error must not hold control characters"),
            "{bad:?}"
        );
    }
}

#[test]
fn validate_refuses_ok_with_an_error_but_allows_fail_with_one() {
    let mut r = sample_report();
    r.nvenc_error = Some("unexpected".into());
    assert_eq!(
        r.validate(),
        Err("nvenc is ok, so nvenc_error must be null")
    );
    r.nvenc = "fail".into();
    assert!(r.validate().is_ok());
}

#[test]
fn the_probe_never_follows_a_redirect_with_its_token() {
    let ci = probe_cloud_init(
        "https://matrix.example.org/_mm/webhooks/fleet/boot-report",
        &"a".repeat(64),
    );
    assert!(
        ci.contains("class NoRedirect(urllib.request.HTTPRedirectHandler):"),
        "{ci}"
    );
    assert!(
        ci.contains("opener = urllib.request.build_opener(NoRedirect)"),
        "{ci}"
    );
    assert!(ci.contains("opener.open(req, timeout=20)"), "{ci}");
    assert!(ci.contains("ensure_ascii=False).encode('utf-8')"), "{ci}");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "minted token")]
fn probe_cloud_init_refuses_a_token_that_was_not_minted() {
    probe_cloud_init(
        "https://matrix.example.org/_mm/webhooks/fleet/boot-report",
        "not-a-token",
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "report_url()")]
fn probe_cloud_init_refuses_a_url_with_a_quote() {
    probe_cloud_init("https://matrix.example.org/\"x", &"a".repeat(64));
}
