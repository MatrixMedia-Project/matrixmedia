//! The runner as a process (`app::run_with`), against a real database: `/metrics` while a
//! standby waits for the lock, a SIGTERM at the worst moment, a database that is not migrated
//! yet, and how a run ends.
//!
//! Alone in its file: the leader lock is one per database, and nothing else may hold it while
//! these run. The tests in this file take turns.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet_runner::app::{LockEnd, finish_run};
use mm_fleet_runner::keyfile;
use mm_fleet_runner::leader::{self, LEADER_LOCK_KEY, LeaderHandle};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};

fn turn() -> &'static Mutex<()> {
    static T: OnceLock<Mutex<()>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(()))
}

/// The pool, the database's URL, and the file-wide turn; the schema is migrated and nothing is
/// left over for the runner's first tick to find (no mode set, no provider, no heartbeat).
async fn setup() -> Option<(PgPool, String, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
    let url = std::env::var("MM_DATABASE_URL").ok()?;
    let guard = turn().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    // The previous test's runner or lock may still be going: a closed connection frees its lock
    // a moment later, once the server notices.
    let deadline = Instant::now() + Duration::from_secs(10);
    while lock_holder(&pool).await.is_some() {
        assert!(
            Instant::now() < deadline,
            "something else holds the leader lock"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
        "mm_fleet_control",
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
    Some((pool, url, guard))
}

/// The real binary. Killed if the test dies before it has exited.
struct Runner {
    child: Child,
    dir: tempfile::TempDir,
}

impl Runner {
    /// Starts `mm-fleet-runner <command>` against `url`, with its key file at `key.json` in
    /// `dir`. Its stderr (the JSON log) goes to `stderr.log` there.
    fn start(dir: tempfile::TempDir, url: &str, command: &str, listen: Option<SocketAddr>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mm-fleet-runner"));
        cmd.arg(command)
            .env("MM_FLEET_RUNNER_DATABASE_URL", url)
            .env("MM_FLEET_RUNNER_KEY_FILE", dir.path().join("key.json"))
            .env_remove("MM_FLEET_TFVARS_PATH")
            .env_remove("MM_FLEET_RUNNER_LISTEN")
            .env_remove("RUST_LOG")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.path().join("stderr.log")).unwrap());
        if let Some(addr) = listen {
            cmd.env("MM_FLEET_RUNNER_LISTEN", addr.to_string());
        }
        let child = cmd.spawn().expect("start the runner");
        Self { child, dir }
    }

    fn key_file(&self) -> std::path::PathBuf {
        self.dir.path().join("key.json")
    }

    fn log_path(&self) -> std::path::PathBuf {
        self.dir.path().join("stderr.log")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }

    fn sigterm(&self) {
        let sent = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill");
        assert!(sent.success());
    }

    /// Waits for the process to end, for at most `secs`.
    async fn exit_within(&mut self, secs: u64) -> Option<ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Polls `f` until it yields `Some`, for at most `secs`. A runner that exits meanwhile fails
    /// the test with its log: whatever was waited for will not happen.
    async fn wait_for<T, F, Fut>(&mut self, what: &str, secs: u64, mut f: F) -> T
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = f().await {
                return v;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "the runner exited ({status}) before {what}:\n{}",
                    self.log()
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The session that holds the leader lock in this database, if any.
async fn lock_holder(pool: &PgPool) -> Option<i32> {
    sqlx::query_scalar(
        "SELECT pid FROM pg_locks
          WHERE locktype = 'advisory' AND granted AND objsubid = 1
            AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
            AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(LEADER_LOCK_KEY)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// An address nothing is listening on: bound to port 0, then released.
async fn free_addr() -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
}

async fn scrape(addr: SocketAddr) -> Option<(reqwest::StatusCode, String)> {
    let r = reqwest::get(format!("http://{addr}/metrics")).await.ok()?;
    let status = r.status();
    Some((status, r.text().await.ok()?))
}

// ---- the command line -----------------------------------------------------------------------

#[test]
fn the_command_line_offers_the_three_commands() {
    let out = Command::new(env!("CARGO_BIN_EXE_mm-fleet-runner"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for command in ["run", "fingerprint", "rotate-key"] {
        assert!(
            help.lines().any(|l| l.trim_start().starts_with(command)),
            "{command} is missing from:\n{help}"
        );
    }
}

#[test]
fn the_fingerprint_command_prints_the_key_fingerprint_and_ends() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("key.json");
    let out = Command::new(env!("CARGO_BIN_EXE_mm-fleet-runner"))
        .arg("fingerprint")
        // Required by the environment check even though this command never connects.
        .env("MM_FLEET_RUNNER_DATABASE_URL", "postgres://unused")
        .env("MM_FLEET_RUNNER_KEY_FILE", &key)
        .env_remove("MM_FLEET_RUNNER_LISTEN")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let kp = keyfile::load_or_create(&key).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        mm_fleet::sealed::display_fingerprint(&kp.fingerprint())
    );
}

// ---- /metrics before the lock ---------------------------------------------------------------

#[tokio::test]
async fn a_standby_serves_metrics_while_it_waits_for_the_lock_and_stops_on_sigterm() {
    let Some((pool, url, _g)) = setup().await else {
        return;
    };
    // Another runner leads.
    let leader = leader::try_acquire(&pool)
        .await
        .unwrap()
        .expect("the lock is free");
    let addr = free_addr().await;
    let mut runner = Runner::start(tempfile::tempdir().unwrap(), &url, "run", Some(addr));

    // It answers, though it cannot lead: the compose health check asks this address, and a
    // standby must pass it too.
    let (status, body) = runner
        .wait_for("/metrics to answer", 30, || async { scrape(addr).await })
        .await;
    assert_eq!(status, 200);
    let log_path = runner.log_path();
    runner
        .wait_for("the runner to start waiting for the lock", 30, || async {
            std::fs::read_to_string(&log_path)
                .is_ok_and(|log| log.contains("holds the leader lock; waiting"))
                .then_some(())
        })
        .await;
    let (status, standby_body) = scrape(addr).await.expect("still answering while it waits");
    assert_eq!(status, 200);
    for text in [&body, &standby_body] {
        // Never beat: nothing ran under a lock it does not hold.
        assert!(
            text.contains("mm_fleet_runner_heartbeat_timestamp 0\n"),
            "{text}"
        );
        assert!(
            text.contains("mm_fleet_provider_check_seconds_count 0\n"),
            "{text}"
        );
    }
    assert!(!runner.key_file().exists(), "a standby does not mint a key");

    // docker stop: it ends at once, and cleanly, not after the grace period and a SIGKILL.
    let asked = Instant::now();
    runner.sigterm();
    let status = runner
        .exit_within(5)
        .await
        .unwrap_or_else(|| panic!("a standby ignored SIGTERM:\n{}", runner.log()));
    assert_eq!(status.code(), Some(0), "{}", runner.log());
    assert!(
        asked.elapsed() < Duration::from_secs(3),
        "took {:?} to stop",
        asked.elapsed()
    );
    assert!(
        runner
            .log()
            .contains("stopped while waiting for the leader lock"),
        "{}",
        runner.log()
    );
    leader.release().await;
}

#[tokio::test]
async fn a_metrics_address_that_cannot_be_bound_is_logged_and_the_runner_carries_on() {
    let Some((pool, url, _g)) = setup().await else {
        return;
    };
    // The address is taken for the whole test.
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = taken.local_addr().unwrap();
    let mut runner = Runner::start(tempfile::tempdir().unwrap(), &url, "run", Some(addr));

    // It still takes the lock and runs.
    runner
        .wait_for("the runner to take the lock", 30, || async {
            lock_holder(&pool).await
        })
        .await;
    let log = runner.log();
    let line = log
        .lines()
        .find(|l| l.contains("cannot serve /metrics"))
        .unwrap_or_else(|| panic!("the bind failure is not logged:\n{log}"));
    assert!(line.contains(&addr.to_string()), "{line}");
    assert!(line.contains("\"level\":\"ERROR\""), "{line}");
    assert!(runner.child.try_wait().unwrap().is_none(), "{log}");

    runner.sigterm();
    let status = runner.exit_within(10).await.expect("stops on SIGTERM");
    assert_eq!(status.code(), Some(0), "{}", runner.log());
    drop(taken);
}

// ---- the signal ----------------------------------------------------------------------------

/// A SIGTERM that arrives after the runner took the lock and before the loops started must end
/// the run all the same. The key file is a FIFO nobody has written yet, so the runner stands
/// in `load_or_create`, between the lock and the loops, for as long as the test wants; the signal
/// is sent there. With one listener for the whole run it is waiting when the loops have started;
/// a second listener, made after the loops start, would never hear it.
#[tokio::test]
async fn a_sigterm_that_lands_while_the_key_loads_still_ends_the_run() {
    let Some((pool, url, _g)) = setup().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("key.json");
    let made = Command::new("mkfifo")
        .args(["-m", "600"])
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(made.success());
    assert_eq!(
        std::fs::metadata(&fifo).unwrap().permissions().mode() & 0o077,
        0
    );
    // A real key, to be written into the FIFO once the signal is in.
    let key_json = {
        let spare = dir.path().join("spare.json");
        keyfile::load_or_create(&spare).unwrap();
        std::fs::read(&spare).unwrap()
    };
    let mut runner = Runner::start(dir, &url, "run", None);

    // The lock is ours to see: the runner took it and is now blocked on the key file.
    runner
        .wait_for("the runner to take the lock", 30, || async {
            lock_holder(&pool).await
        })
        .await;
    runner.sigterm();
    // Let the runner's key read through; it has not seen the signal yet as far as it can tell.
    let fifo_path = runner.key_file();
    std::thread::spawn(move || {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&fifo_path) {
            let _ = f.write_all(&key_json);
        }
    });

    let status = runner.exit_within(20).await.unwrap_or_else(|| {
        panic!(
            "the SIGTERM sent while the key was loading was lost:\n{}",
            runner.log()
        )
    });
    assert_eq!(status.code(), Some(0), "{}", runner.log());
    let log = runner.log();
    assert!(log.contains("leader lock acquired"), "{log}");
    assert!(log.contains("runner key loaded"), "{log}");
    // And it let the lock go: the next runner does not wait for the server to notice a socket.
    let freed = leader::try_acquire(&pool).await.unwrap();
    freed
        .expect("the lock is free once the runner has ended")
        .release()
        .await;
}

// ---- the schema -----------------------------------------------------------------------------

/// `with_database("postgres://u:p@h:5432/db?x=y", "other")` is the same server's `other`.
fn with_database(url: &str, name: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let (server, _) = base.rsplit_once('/').expect("a database name in the URL");
    match query {
        Some(q) => format!("{server}/{name}?{q}"),
        None => format!("{server}/{name}"),
    }
}

#[tokio::test]
async fn a_database_without_v042_ends_the_runner_with_status_1() {
    let Some((pool, url, _g)) = setup().await else {
        return;
    };
    // A database of its own, so that the tables the runner finds are the ones this test makes.
    let name = "mm_runner_app_schema";
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&pool)
        .await
        .unwrap();
    let scratch_url = with_database(&url, name);
    let scratch = sqlx::PgPool::connect(&scratch_url).await.unwrap();

    let ends_with_1 = |what: &'static str, command: &'static str| {
        let scratch_url = scratch_url.clone();
        async move {
            let mut runner =
                Runner::start(tempfile::tempdir().unwrap(), &scratch_url, command, None);
            let status = runner
                .exit_within(15)
                .await
                .unwrap_or_else(|| panic!("{what}: the runner went on:\n{}", runner.log()));
            assert_eq!(status.code(), Some(1), "{what}:\n{}", runner.log());
            assert!(
                runner
                    .log()
                    .contains("V042 not applied yet; start mm-core first"),
                "{what}:\n{}",
                runner.log()
            );
            assert!(
                !runner.key_file().exists(),
                "{what}: it must not have started"
            );
        }
    };

    // No table at all.
    ends_with_1("no tables", "run").await;
    // The tables of V041: the requests table is there, without the column V042 adds last.
    sqlx::query("CREATE TABLE mm_fleet_control (id INT PRIMARY KEY)")
        .execute(&scratch)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE mm_fleet_requests (id TEXT PRIMARY KEY)")
        .execute(&scratch)
        .await
        .unwrap();
    ends_with_1("no params column", "run").await;
    ends_with_1("no params column, rotate-key", "rotate-key").await;

    scratch.close().await;
    sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .execute(&pool)
        .await
        .unwrap();
}

// ---- the end of a run -----------------------------------------------------------------------

#[tokio::test]
async fn the_lock_is_released_once_the_loops_have_stopped() {
    let Some((pool, _url, _g)) = setup().await else {
        return;
    };
    let lock = leader::try_acquire(&pool).await.unwrap().expect("free");
    let loops = tokio::spawn(async {});
    let end = finish_run(
        loops,
        std::sync::Arc::new(LeaderHandle::new(lock)),
        false,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(end, LockEnd::Released);
    // Free at once, not once the server notices a closed socket.
    leader::try_acquire(&pool)
        .await
        .unwrap()
        .expect("a standby can take the lock now")
        .release()
        .await;
}

#[tokio::test]
async fn the_lock_is_not_released_while_a_loop_is_still_running() {
    let Some((pool, _url, _g)) = setup().await else {
        return;
    };
    let lock = leader::try_acquire(&pool).await.unwrap().expect("free");
    // A loop that does not stop (the checks loop in a call a provider never answers) and holds
    // no clone of the lock handle: nothing but the order of events keeps the lock from being
    // released under it.
    let ran = std::sync::Arc::new(AtomicBool::new(false));
    let stuck = tokio::spawn({
        let ran = ran.clone();
        async move {
            ran.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await
        }
    });
    let end = finish_run(
        stuck,
        std::sync::Arc::new(LeaderHandle::new(lock)),
        false,
        Duration::from_millis(300),
    )
    .await
    .unwrap();
    assert!(ran.load(Ordering::SeqCst), "the stand-in loop ran");
    assert_eq!(end, LockEnd::LeftToTheProcess);
}

#[tokio::test]
async fn a_lost_lock_ends_the_run_with_an_error_once_the_loops_have_stopped() {
    let Some((pool, _url, _g)) = setup().await else {
        return;
    };
    let lock = leader::try_acquire(&pool).await.unwrap().expect("free");
    let err = finish_run(
        tokio::spawn(async {}),
        std::sync::Arc::new(LeaderHandle::new(lock)),
        true,
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "lost the leader lock; exiting so a standby can take over"
    );
}
