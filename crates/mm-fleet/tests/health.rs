use std::sync::OnceLock;

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::health::{self, FleetHealth};
use mm_fleet::placement::{self, Limits, PlacementRequest, Skip};
use mm_fleet::placement_db;
use mm_fleet::roles::{Backend, Purpose, Role};
use tokio::sync::{Mutex, MutexGuard};

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Every table the fleet's rows hang off, children first, so a leftover row from another test
/// binary cannot stop a delete.
async fn wipe(pool: &sqlx::PgPool) {
    for t in [
        "mm_fleet_control",
        "mm_fleet_boot_tokens",
        "mm_fleet_zone_cooldown",
        "mm_fleet_desired",
        "mm_fleet_provider_status",
        "mm_fleet_provider_credentials",
        "mm_fleet_requests",
        "mm_fleet_provider_sizes",
        "mm_fleet_provider_zones",
        "mm_fleet_nodes",
        "mm_fleet_providers",
        "mm_fleet_ops_audit",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(pool)
            .await
            .expect("wipe");
    }
}

async fn setup() -> Option<(sqlx::PgPool, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
    let guard = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    wipe(&pool).await;
    Some((pool, guard))
}

/// The runner's heartbeat row, `secs_ago` seconds old by the database's clock (negative: in the
/// future, as after the database's clock stepped back).
async fn runner_heartbeat(pool: &sqlx::PgPool, secs_ago: i64) {
    sqlx::query(
        "INSERT INTO mm_fleet_control (id, runner_version, public_key, key_fingerprint, heartbeat_at, fleet_mode_seen, settings_rev_seen)
         VALUES (1, 't', '\\x00', 'k', now() - make_interval(secs => $1), 'frozen', 0)",
    )
    .bind(secs_ago as f64)
    .execute(pool)
    .await
    .unwrap();
}

/// A rented machine, its deadline `deadline_in_secs` from the database's now (negative: past).
async fn rented(
    pool: &sqlx::PgPool,
    id: &str,
    state: &str,
    created_backend: Option<&str>,
    deadline_in_secs: i64,
) {
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, created_backend, destroy_deadline)
         VALUES ($1, 'transcode', 'rented', 'scaleway', $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(id)
    .bind(state)
    .bind(created_backend)
    .bind(deadline_in_secs as f64)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn no_runner_ever_reads_as_minus_one_and_nothing_rented_as_zero() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let h = health::sample(&pool).await.unwrap();
    assert_eq!(
        h,
        FleetHealth {
            heartbeat_age_secs: -1,
            ..Default::default()
        }
    );
}

#[tokio::test]
async fn a_stale_runner_with_a_rented_machine_past_its_deadline_is_visible() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    runner_heartbeat(&pool, 300).await;
    // Past its deadline by 3 minutes, and by 10 (still `destroying`: a failed destroy).
    rented(&pool, "tb-late", "healthy", Some("api"), -180).await;
    rented(&pool, "tb-later", "destroying", Some("api"), -600).await;
    // Not an overrun: closed out an hour ago; not yet due; made through Terraform; owned.
    rented(&pool, "tb-done", "gone", Some("api"), -3600).await;
    rented(&pool, "tb-early", "booting", Some("api"), 3600).await;
    rented(&pool, "tb-tf", "healthy", Some("terraform"), -7200).await;
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state) VALUES ('own-1', 'origin', 'owned', 'colo', 'healthy')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let h = health::sample(&pool).await.unwrap();
    assert!((295..=315).contains(&h.heartbeat_age_secs), "{h:?}");
    assert_eq!(
        h.rented_live, 4,
        "rented and not gone, whoever made it; an owned machine and a gone one do not count: {h:?}"
    );
    assert!(
        (595..=615).contains(&h.max_overrun_secs),
        "the largest overrun among API-made, not-gone machines past their deadline: {h:?}"
    );
}

#[tokio::test]
async fn a_machine_before_its_deadline_is_no_overrun() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    rented(&pool, "tb-early", "healthy", Some("api"), 3600).await;
    let h = health::sample(&pool).await.unwrap();
    assert_eq!(h.rented_live, 1);
    assert_eq!(h.max_overrun_secs, 0, "time left is not an overrun: {h:?}");
}

#[tokio::test]
async fn a_heartbeat_ahead_of_the_clock_is_not_mistaken_for_no_runner() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    runner_heartbeat(&pool, -3600).await;
    let h = health::sample(&pool).await.unwrap();
    assert_eq!(
        h.heartbeat_age_secs, 0,
        "only -1 means the runner never reported: {h:?}"
    );
}

/// One provider's row, token and verdict, each timed by the database's clock.
struct Case {
    name: &'static str,
    kind: &'static str,
    enabled: bool,
    deleted: bool,
    /// Seconds since the token was entered; `None`: no token.
    token_age: Option<i64>,
    /// The verdict's state and the seconds since it was checked; `None`: never checked.
    verdict: Option<(&'static str, i64)>,
    /// The verdict's `last_error_kind`.
    error_kind: Option<&'static str>,
    /// The expected contribution to `providers_need_attention`.
    attention: i64,
    /// The expected contribution to `providers_unverified`.
    unverified: i64,
}

async fn add_provider(pool: &sqlx::PgPool, id: &str, priority: i32, c: &Case) {
    sqlx::query(
        "INSERT INTO mm_fleet_providers (id, label, kind, enabled, priority, endpoint_display, image, gpu_image, deleted_at)
         VALUES ($1, $1, $5, $2, $3, 'https://api.example.net', 'i', 'g',
                 CASE WHEN $4 THEN now() ELSE NULL END)",
    )
    .bind(id)
    .bind(c.enabled)
    .bind(priority)
    .bind(c.deleted)
    .bind(c.kind)
    .execute(pool)
    .await
    .unwrap();
    if let Some(age) = c.token_age {
        sqlx::query(
            "INSERT INTO mm_fleet_provider_credentials (provider_id, key_id, enc, ciphertext, entered_by, entered_at)
             VALUES ($1, 'k', '\\x01', '\\x02', '@argi:example', now() - make_interval(secs => $2))",
        )
        .bind(id)
        .bind(age as f64)
        .execute(pool)
        .await
        .unwrap();
    }
    if let Some((state, age)) = c.verdict {
        sqlx::query(
            "INSERT INTO mm_fleet_provider_status (provider_id, checked_at, state, last_error_kind)
             VALUES ($1, now() - make_interval(secs => $2), $3, $4)",
        )
        .bind(id)
        .bind(age as f64)
        .bind(state)
        .bind(c.error_kind)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn cases() -> Vec<Case> {
    let live = |name, token_age, verdict, attention, unverified| Case {
        name,
        kind: "scaleway",
        enabled: true,
        deleted: false,
        token_age,
        verdict,
        error_kind: None,
        attention,
        unverified,
    };
    vec![
        live(
            "an ok verdict newer than the token and fresh",
            Some(600),
            Some(("ok", 60)),
            0,
            0,
        ),
        live(
            "an ok verdict that judged an older token",
            Some(60),
            Some(("ok", 120)),
            0,
            1,
        ),
        live(
            "an ok verdict just inside the freshness window",
            Some(3000),
            Some(("ok", 850)),
            0,
            0,
        ),
        live(
            "an ok verdict just outside it",
            Some(3000),
            Some(("ok", 950)),
            0,
            1,
        ),
        live("a token and no verdict yet", Some(60), None, 0, 1),
        live(
            "an unknown verdict, fresh",
            Some(600),
            Some(("unknown", 30)),
            0,
            1,
        ),
        live(
            "waiting for the runner to read the token",
            Some(600),
            Some(("waiting_for_token", 30)),
            0,
            1,
        ),
        live(
            "a token the provider rejected",
            Some(600),
            Some(("needs_you", 30)),
            1,
            0,
        ),
        live(
            "an endpoint that changed",
            Some(600),
            Some(("endpoint_mismatch", 30)),
            1,
            0,
        ),
        live(
            "a rejected token whose verdict has gone stale",
            Some(3000),
            Some(("needs_you", 2000)),
            1,
            0,
        ),
        live(
            "no token: nothing to verify",
            None,
            Some(("waiting_for_token", 30)),
            0,
            0,
        ),
        Case {
            name: "disabled, token rejected",
            enabled: false,
            ..live("", Some(600), Some(("needs_you", 30)), 0, 0)
        },
        Case {
            name: "disabled, no verdict",
            enabled: false,
            ..live("", Some(600), None, 0, 0)
        },
        Case {
            name: "deleted, token rejected",
            deleted: true,
            ..live("", Some(600), Some(("needs_you", 30)), 0, 0)
        },
        Case {
            name: "deleted, no verdict",
            deleted: true,
            ..live("", Some(600), None, 0, 0)
        },
        // A kind with no checks yet is recorded as `unknown` / `unsupported` on every pass and
        // can never become verified; nothing is wrong with it, so it must never count.
        Case {
            name: "a kind whose checks are not built yet",
            kind: "gcp",
            error_kind: Some("unsupported"),
            ..live("", Some(600), Some(("unknown", 30)), 0, 0)
        },
        Case {
            name: "the same, its record gone stale",
            kind: "gcp",
            error_kind: Some("unsupported"),
            ..live("", Some(3000), Some(("unknown", 2000)), 0, 0)
        },
        // Before the runner's first pass nothing has said the checks are not built: counted.
        Case {
            name: "a kind whose checks are not built yet, not checked once",
            kind: "gcp",
            ..live("", Some(60), None, 0, 1)
        },
        // Only `unsupported` is excused: a provider that cannot be reached is a real problem.
        Case {
            name: "an unknown verdict because the provider was unreachable",
            error_kind: Some("transient"),
            ..live("", Some(600), Some(("unknown", 30)), 0, 1)
        },
    ]
}

#[tokio::test]
async fn providers_needing_a_human_or_without_a_fresh_verdict_are_counted() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    for c in cases() {
        wipe(&pool).await;
        add_provider(&pool, "p-one", 1, &c).await;
        let h = health::sample(&pool).await.unwrap();
        assert_eq!(
            (h.providers_need_attention, h.providers_unverified),
            (c.attention, c.unverified),
            "{}: (need attention, unverified)",
            c.name
        );
    }

    // All at once, each counted in at most one of the two.
    wipe(&pool).await;
    let all = cases();
    for (i, c) in all.iter().enumerate() {
        add_provider(&pool, &format!("p-{i:02}"), i as i32 + 1, c).await;
    }
    let h = health::sample(&pool).await.unwrap();
    assert_eq!(
        h.providers_need_attention,
        all.iter().map(|c| c.attention).sum::<i64>()
    );
    assert_eq!(
        h.providers_unverified,
        all.iter().map(|c| c.unverified).sum::<i64>()
    );
}

/// The gauge and the rental decision must not drift apart: a provider this counts as unverified
/// is exactly one placement refuses as `NotVerified`, read through the same facts the runner's
/// rental loop reads, less the two groups the gauge sets aside on purpose: those already counted
/// as needing a human, and those whose kind has no checks yet (the runner records them as
/// `unsupported`; placement refuses them too, rightly, but nothing is wrong).
#[tokio::test]
async fn unverified_is_what_placement_refuses_as_not_verified() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let all = cases();
    let mut unbuilt = Vec::new();
    for (i, c) in all.iter().enumerate() {
        let id = format!("p-{i:02}");
        add_provider(&pool, &id, i as i32 + 1, c).await;
        if c.error_kind == Some("unsupported") {
            unbuilt.push(id);
        }
    }
    assert!(
        !unbuilt.is_empty(),
        "the fixture must include a kind without checks"
    );
    let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (facts, live) = placement_db::load_facts(&pool).await.unwrap();
    let req = PlacementRequest {
        role: Role::Transcode,
        region: "eu".into(),
        purpose: Purpose::Broadcast,
        backend: Backend::Api,
        now,
    };
    let limits = Limits {
        max_gpu_nodes: 100,
        gpu_nodes_live: live,
    };
    let refused: Vec<String> = placement::eligible(&facts, &req, &limits)
        .excluded
        .into_iter()
        .filter(|e| e.zone.is_none() && e.reason == Skip::NotVerified)
        .map(|e| e.provider_id)
        .filter(|id| {
            let state = facts
                .iter()
                .find(|f| &f.id == id)
                .and_then(|f| f.status_state.as_deref());
            !matches!(state, Some("needs_you" | "endpoint_mismatch")) && !unbuilt.contains(id)
        })
        .collect();
    assert!(!refused.is_empty(), "the fixture must exercise the rule");

    let h = health::sample(&pool).await.unwrap();
    assert_eq!(
        h.providers_unverified,
        refused.len() as i64,
        "placement refuses {refused:?} as not verified"
    );
}

#[test]
fn publish_sets_every_gauge() {
    use mm_core::metrics_global as g;

    health::publish(&FleetHealth {
        heartbeat_age_secs: -1,
        rented_live: 3,
        max_overrun_secs: 421,
        providers_need_attention: 5,
        providers_unverified: 7,
    });
    assert_eq!(g::FLEET_RUNNER_HEARTBEAT_AGE.get(), -1);
    assert_eq!(g::FLEET_RENTED_NODES_LIVE.get(), 3);
    assert_eq!(g::FLEET_NODE_OVERRUN.get(), 421);
    assert_eq!(g::FLEET_PROVIDERS_NEED_ATTENTION.get(), 5);
    assert_eq!(g::FLEET_PROVIDERS_UNVERIFIED.get(), 7);

    // The next sample replaces the last, in both directions.
    health::publish(&FleetHealth {
        heartbeat_age_secs: 12,
        ..Default::default()
    });
    assert_eq!(g::FLEET_RUNNER_HEARTBEAT_AGE.get(), 12);
    assert_eq!(g::FLEET_RENTED_NODES_LIVE.get(), 0);
    assert_eq!(g::FLEET_NODE_OVERRUN.get(), 0);
    assert_eq!(g::FLEET_PROVIDERS_NEED_ATTENTION.get(), 0);
    assert_eq!(g::FLEET_PROVIDERS_UNVERIFIED.get(), 0);
}

/// An alert that selects a name nothing exports can never fire (the alert file records this bug
/// twice), so every fleet metric the GPU group uses must be exported by mm-core's gauges or by
/// the runner's `/metrics`.
#[test]
fn the_gpu_alerts_select_only_series_something_exports() {
    let file = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../infra/prometheus/matrixmedia-alerts.yml"
    ))
    .expect("the alert rules");
    let group = file
        .split("- name: matrixmedia_fleet_gpu")
        .nth(1)
        .expect("the GPU group exists");
    let group = group.split("\n  - name: ").next().unwrap();

    let mut used = std::collections::BTreeSet::new();
    let mut rest = group;
    while let Some(at) = rest.find("mm_fleet_") {
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        used.insert(tail[..end].to_string());
        rest = &tail[end..];
    }
    assert!(used.len() >= 6, "the group selects on {used:?}");

    let core = prometheus::Registry::new();
    mm_core::metrics_global::register_all(&core).unwrap();
    let runner = prometheus::Registry::new();
    mm_fleet::metrics::register_runner(&runner).unwrap();
    // A labelled family is exported only once it has a child.
    mm_fleet::metrics::count_create("p-alerts-test", "z-alerts-test", "ok");
    let exported: std::collections::BTreeSet<String> = core
        .gather()
        .iter()
        .chain(runner.gather().iter())
        .map(|f| f.get_name().to_string())
        .collect();
    for name in &used {
        assert!(
            exported.contains(name),
            "the GPU alerts select {name}, which nothing exports (exported: {exported:?})"
        );
    }
}
