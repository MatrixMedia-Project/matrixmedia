//! The real binary, against a real database: when its leader lock is lost it stops its loops
//! and exits with status 1, so a supervisor restarts it and a standby can take over. A clean
//! stop (a signal) is a different exit; this test is about the lock.
//!
//! Alone in its file: the leader lock is one per database, and nothing else may hold it while
//! this runs.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet_runner::leader::LEADER_LOCK_KEY;

/// Kills the child if the test dies before it has exited.
struct Reaper(Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn the_runner_binary_exits_with_status_1_when_its_leader_lock_is_lost() {
    let Some(pool) = try_pool().await else {
        return;
    };
    let Ok(url) = std::env::var("MM_DATABASE_URL") else {
        return;
    };
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    // Nothing for the runner's first tick to find, and no mode left over from another file.
    for t in [
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
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&pool)
            .await
            .expect("wipe");
    }
    sqlx::query("DELETE FROM mm_settings WHERE key LIKE 'fleet.%'")
        .execute(&pool)
        .await
        .expect("wipe settings");
    sqlx::query("DELETE FROM mm_fleet_control")
        .execute(&pool)
        .await
        .expect("wipe the heartbeat");

    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("stderr.log");
    let child = Command::new(env!("CARGO_BIN_EXE_mm-fleet-runner"))
        .arg("run")
        .env("MM_FLEET_RUNNER_DATABASE_URL", &url)
        .env("MM_FLEET_RUNNER_KEY_FILE", dir.path().join("key.json"))
        .env_remove("MM_FLEET_TFVARS_PATH")
        .env_remove("MM_FLEET_RUNNER_LISTEN")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log_path).unwrap())
        .spawn()
        .expect("start the runner");
    let mut runner = Reaper(child);

    // The runner takes the lock on a connection of its own; find that session.
    let started = Instant::now();
    let holder: i32 = loop {
        let pid: Option<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_locks
              WHERE locktype = 'advisory' AND granted AND objsubid = 1
                AND ((classid::bigint << 32) | objid::bigint) = $1",
        )
        .bind(LEADER_LOCK_KEY)
        .fetch_optional(&pool)
        .await
        .unwrap();
        if let Some(pid) = pid {
            break pid;
        }
        assert!(
            runner.0.try_wait().unwrap().is_none(),
            "the runner exited before it took the lock: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the runner never took the leader lock"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // Let the loops start and the fleet loop take its first tick under the lock (the heartbeat
    // is the sign they are up), so that the lock is lost while the runner is running.
    loop {
        let beats: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_control")
            .fetch_one(&pool)
            .await
            .unwrap();
        if beats > 0 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the runner never beat"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // The lock is lost: Postgres drops the session that holds it, as a failover or a network
    // partition would.
    let ended: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(holder)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(ended);

    // The fleet loop notices on its next tick (every 10 s), stops every loop, and the process
    // exits non-zero.
    let waited = Instant::now();
    let status = loop {
        if let Some(status) = runner.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            waited.elapsed() < Duration::from_secs(40),
            "the runner kept running after it lost the leader lock"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(log.contains("lost the leader lock"), "{log}");
    // The exit came after every loop had stopped, not before.
    let stopped = log
        .find("every loop has stopped")
        .unwrap_or_else(|| panic!("{log}"));
    let lost = log
        .find("lost the leader lock; stopping every loop")
        .unwrap_or_else(|| panic!("{log}"));
    assert!(lost < stopped, "{log}");
}
