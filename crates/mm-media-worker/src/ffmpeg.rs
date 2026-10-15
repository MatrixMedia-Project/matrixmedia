//! Running ffmpeg and ffprobe on hostile bytes, inside a container whose memory and pids are
//! shared by every job loop. So every child is:
//!
//! - **in its own process group**, killed as a group on a timeout or a stop request (spec §5.9)
//!   and, as a backstop, when the future is dropped (`kill_on_drop` and a group guard);
//! - **reaped before we return** on a timeout or a stop, so no zombie holds a pid slot;
//! - **given a scrubbed environment** (`PATH`, `LANG`, `LC_ALL` only): the database URL must not
//!   sit in the environment of the process that parses upload bytes;
//! - **read with bounded memory**: stderr into a ring of the last [`STDERR_TAIL`] bytes, `capture`
//!   stdout up to [`CAPTURE_STDOUT_MAX`], progress lines up to [`PROGRESS_LINE_MAX`]. Output past a
//!   bound is still drained (the child never blocks on a full pipe) but not kept.

use std::ffi::OsStr;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub enum RunError {
    /// The program could not be started (or its status could not be read).
    Spawn(std::io::Error),
    Timeout,
    Stopped,
    /// `capture` stdout passed [`CAPTURE_STDOUT_MAX`]. Truncated ffprobe JSON cannot be parsed, so
    /// callers treat this like a failed probe: a corrupt or hostile source.
    OutputTooLarge,
    /// The child ran and failed. `signal` is set when it was killed by a signal (9 = the OOM
    /// killer, transient); otherwise `code` is its exit code (127 = program not found).
    Failed { status: String, code: Option<i32>, signal: Option<i32>, stderr_tail: String },
}

#[derive(Debug)]
pub struct Captured {
    pub stdout: String,
    /// At most the last [`STDERR_TAIL`] bytes, prefixed with `…` when earlier output was dropped.
    pub stderr: String,
}

/// How much of a child's stderr is kept (a failure message, or a loudnorm JSON block at the end).
pub const STDERR_TAIL: usize = 2048;
/// The most `capture` keeps of stdout (ffprobe JSON is a few KiB; this is the hostile-input cap).
pub const CAPTURE_STDOUT_MAX: usize = 8 * 1024 * 1024;
/// The longest `-progress` line that is parsed; a longer one is discarded whole.
pub const PROGRESS_LINE_MAX: usize = 4096;

const CHUNK: usize = 8 * 1024;
/// How long a killed child may take to die and be reaped, and how long the output pipes may stay
/// open after the child itself has exited.
const REAP_GRACE: Duration = Duration::from_secs(2);
/// A timer that is parked until it is re-armed.
const PARKED: Duration = Duration::from_secs(86_400);
const ELLIPSIS: &str = "…";
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// A short command (ffprobe, a loudness pass) run to completion.
pub async fn capture(program: &Path, args: &[String], timeout: Duration, stop: &CancellationToken) -> Result<Captured, RunError> {
    let mut cmd = command(program);
    cmd.args(args);
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let mut out: Vec<u8> = Vec::new();
    let mut truncated = false;
    let (status, stderr) = supervise(child, stdout, stderr, timeout, stop, |chunk| {
        let room = CAPTURE_STDOUT_MAX - out.len();
        if chunk.len() > room {
            truncated = true;
        }
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
    })
    .await?;
    if !status.success() {
        return Err(failed(status, stderr));
    }
    if truncated {
        return Err(RunError::OutputTooLarge);
    }
    let stdout = String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    Ok(Captured { stdout, stderr })
}

/// An encode under `nice -n {nice}`, reporting ffmpeg's `-progress pipe:1` position in ms.
pub async fn encode(
    program: &Path,
    args: &[String],
    nice: i32,
    timeout: Duration,
    stop: &CancellationToken,
    mut on_progress: impl FnMut(u64),
) -> Result<(), RunError> {
    let mut cmd = if nice > 0 {
        let mut c = command("nice");
        c.arg("-n").arg(nice.to_string()).arg(program);
        c
    } else {
        command(program)
    };
    cmd.args(args);
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let mut lines = LineSplitter::default();
    let mut report = |line: &str| {
        if let Some(ms) = progress_ms(line) {
            on_progress(ms);
        }
    };
    let (status, stderr) = supervise(child, stdout, stderr, timeout, stop, |chunk| lines.feed(chunk, &mut report)).await?;
    lines.finish(&mut report);
    if status.success() { Ok(()) } else { Err(failed(status, stderr)) }
}

/// `out_time_us=12345678` → 12 345 ms. (ffmpeg's `out_time_ms` key is also microseconds — a
/// long-standing misnomer — so only `out_time_us` is read.)
pub fn progress_ms(line: &str) -> Option<u64> {
    line.strip_prefix("out_time_us=")?
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|v| *v >= 0)
        .map(|us| us as u64 / 1000)
}

/// The last `max` bytes of `s`, cut on a character boundary.
pub fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

fn failed(status: ExitStatus, stderr_tail: String) -> RunError {
    RunError::Failed { status: status.to_string(), code: status.code(), signal: status.signal(), stderr_tail }
}

/// A child with piped output, its own process group and a scrubbed environment.
fn command(program: impl AsRef<OsStr>) -> Command {
    let path = std::env::var_os("PATH").filter(|p| !p.is_empty()).unwrap_or_else(|| DEFAULT_PATH.into());
    let mut c = Command::new(program);
    c.env_clear().env("PATH", path);
    for key in ["LANG", "LC_ALL"] {
        if let Some(v) = std::env::var_os(key) {
            c.env(key, v);
        }
    }
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).process_group(0);
    c
}

/// Runs `child` to completion, feeding its stdout to `on_stdout` and keeping the tail of its
/// stderr. Ends early, with the child's whole process group killed and the child reaped, on the
/// `timeout` (wall clock) or when `stop` is cancelled.
async fn supervise(
    mut child: Child,
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
    timeout: Duration,
    stop: &CancellationToken,
    mut on_stdout: impl FnMut(&[u8]),
) -> Result<(ExitStatus, String), RunError> {
    let mut guard = GroupGuard::new(&child);
    let deadline = tokio::time::sleep(timeout);
    // Re-armed to `REAP_GRACE` when the child exits: pipes still open after that are held by a
    // descendant, which is killed rather than waited for.
    let grace = tokio::time::sleep(PARKED);
    tokio::pin!(deadline, grace);

    let mut ring = Tail::new(STDERR_TAIL);
    let mut out_buf = [0u8; CHUNK];
    let mut err_buf = [0u8; CHUNK];
    let (mut out_open, mut err_open) = (true, true);
    let mut status: Option<ExitStatus> = None;

    while status.is_none() || out_open || err_open {
        tokio::select! {
            n = next_chunk(&mut stdout, &mut out_buf), if out_open => match n {
                Some(n) => on_stdout(&out_buf[..n]),
                None => out_open = false,
            },
            n = next_chunk(&mut stderr, &mut err_buf), if err_open => match n {
                Some(n) => ring.push(&err_buf[..n]),
                None => err_open = false,
            },
            s = child.wait(), if status.is_none() => {
                let s = s.map_err(RunError::Spawn)?;
                // The guard stays armed: the leader is gone, but a descendant may still hold the
                // pipes, and a future dropped in this window must kill it too.
                status = Some(s);
                grace.as_mut().reset(Instant::now() + REAP_GRACE);
            },
            _ = &mut grace, if status.is_some() => {
                tracing::warn!("a process kept the output pipes open after the child exited; killing its process group");
                guard.kill_group();
                break;
            },
            _ = &mut deadline => {
                reap(&mut child, &mut guard).await;
                return Err(RunError::Timeout);
            },
            _ = stop.cancelled() => {
                reap(&mut child, &mut guard).await;
                return Err(RunError::Stopped);
            },
        }
    }
    // The leader has exited and every pipe is closed (or the stragglers were just killed): nothing
    // is left to guard.
    guard.armed = false;
    Ok((status.expect("the loop only ends with an exit status"), ring.finish()))
}

/// Kills the child's process group, then the child itself, and waits for it (bounded), so the
/// pid is free again before the caller returns.
async fn reap(child: &mut Child, guard: &mut GroupGuard) {
    guard.kill_group();
    let _ = tokio::time::timeout(REAP_GRACE, child.kill()).await;
    guard.armed = false;
}

/// One cancel-safe read from a child pipe. `Some(0)` means "nothing yet, ask again"; `None`
/// means the pipe is finished: at EOF, or after a read error whose remainder was drained to a
/// sink (a child must never block on a full pipe while it runs).
async fn next_chunk<R: AsyncRead + Unpin>(pipe: &mut R, buf: &mut [u8]) -> Option<usize> {
    match pipe.read(buf).await {
        Ok(0) => None,
        Ok(n) => Some(n),
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Some(0),
        Err(_) => {
            let _ = tokio::io::copy(pipe, &mut tokio::io::sink()).await;
            None
        }
    }
}

/// Kills the child's process group if the supervising future is dropped before `supervise` has
/// finished with it (a worker shutdown): `kill_on_drop` alone reaches only the group leader.
struct GroupGuard {
    pgid: Option<Pid>,
    /// Cleared when the loop ends or `reap` has run, not when the leader exits: until then a
    /// descendant may still hold the output pipes, and dropping the future must kill it.
    armed: bool,
}

impl GroupGuard {
    fn new(child: &Child) -> Self {
        let pgid = child.id().and_then(|p| i32::try_from(p).ok()).and_then(Pid::from_raw);
        Self { pgid, armed: true }
    }

    fn kill_group(&self) {
        if let Some(pgid) = self.pgid {
            match kill_process_group(pgid, Signal::KILL) {
                Ok(()) | Err(Errno::SRCH) => {}
                Err(e) => tracing::warn!(error = ?e, "could not kill the child's process group"),
            }
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.kill_group();
        }
    }
}

/// A ring of the last `max` bytes written to it.
struct Tail {
    buf: Vec<u8>,
    max: usize,
    dropped: bool,
}

impl Tail {
    fn new(max: usize) -> Self {
        Self { buf: Vec::new(), max, dropped: false }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= self.max {
            self.dropped |= !self.buf.is_empty() || chunk.len() > self.max;
            self.buf.clear();
            self.buf.extend_from_slice(&chunk[chunk.len() - self.max..]);
            return;
        }
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > self.max {
            let excess = self.buf.len() - self.max;
            self.buf.drain(..excess);
            self.dropped = true;
        }
    }

    /// The kept bytes as text of at most `max` bytes; `…` first when anything was dropped.
    fn finish(self) -> String {
        let mut bytes = &self.buf[..];
        if self.dropped {
            // The cut may have split a character: skip its trailing (continuation) bytes.
            let split = bytes.iter().take(3).take_while(|b| (**b & 0xC0) == 0x80).count();
            bytes = &bytes[split..];
        }
        let text = String::from_utf8_lossy(bytes);
        if !self.dropped && text.len() <= self.max {
            return text.into_owned();
        }
        format!("{ELLIPSIS}{}", tail(&text, self.max - ELLIPSIS.len()))
    }
}

/// Splits a byte stream into lines of at most [`PROGRESS_LINE_MAX`] bytes (a longer line is
/// discarded whole) and decodes them lossily, so a stray byte can never stop the drain.
#[derive(Default)]
struct LineSplitter {
    line: Vec<u8>,
    overlong: bool,
}

impl LineSplitter {
    fn feed(&mut self, mut chunk: &[u8], mut emit: impl FnMut(&str)) {
        while let Some(i) = chunk.iter().position(|b| *b == b'\n') {
            self.extend(&chunk[..i]);
            self.finish(&mut emit);
            chunk = &chunk[i + 1..];
        }
        self.extend(chunk);
    }

    fn extend(&mut self, part: &[u8]) {
        if self.overlong {
            return;
        }
        if self.line.len() + part.len() > PROGRESS_LINE_MAX {
            self.overlong = true;
            self.line.clear();
        } else {
            self.line.extend_from_slice(part);
        }
    }

    /// Ends the current line (at a newline, or at EOF).
    fn finish(&mut self, mut emit: impl FnMut(&str)) {
        if !self.overlong && !self.line.is_empty() {
            emit(&String::from_utf8_lossy(&self.line));
        }
        self.line.clear();
        self.overlong = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["-c".into(), script.into()]
    }

    fn no_stop() -> CancellationToken {
        CancellationToken::new()
    }

    #[tokio::test]
    async fn capture_returns_both_streams() {
        let c = capture(Path::new("sh"), &sh("echo out; echo err >&2"), Duration::from_secs(5), &no_stop()).await.unwrap();
        assert_eq!((c.stdout.trim(), c.stderr.trim()), ("out", "err"));
    }

    #[tokio::test]
    async fn a_failure_keeps_the_stderr_tail() {
        match capture(Path::new("sh"), &sh("echo boom >&2; exit 3"), Duration::from_secs(5), &no_stop()).await {
            Err(RunError::Failed { stderr_tail, code, signal, .. }) => {
                assert!(stderr_tail.contains("boom"));
                assert_eq!((code, signal), (Some(3), None));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_signal_is_not_an_exit_code() {
        match capture(Path::new("sh"), &sh("kill -9 $$"), Duration::from_secs(5), &no_stop()).await {
            Err(RunError::Failed { code, signal, .. }) => assert_eq!((code, signal), (None, Some(9))),
            other => panic!("{other:?}"),
        }
        // The same through `encode` (and `nice`): exit 127 is an exit code.
        match encode(Path::new("sh"), &sh("exit 127"), 5, Duration::from_secs(5), &no_stop(), |_| {}).await {
            Err(RunError::Failed { code, signal, .. }) => assert_eq!((code, signal), (Some(127), None)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn capture_times_out() {
        let t = std::time::Instant::now();
        let r = capture(Path::new("sh"), &sh("sleep 5"), Duration::from_millis(100), &no_stop()).await;
        assert!(matches!(r, Err(RunError::Timeout)));
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn encode_reports_progress_under_nice() {
        let mut seen = Vec::new();
        encode(
            Path::new("sh"),
            &sh("printf 'out_time_us=1000000\\nprogress=continue\\nout_time_us=2500000\\nprogress=end\\n'"),
            5,
            Duration::from_secs(5),
            &no_stop(),
            |ms| seen.push(ms),
        )
        .await
        .unwrap();
        assert_eq!(seen, vec![1000, 2500]);
    }

    #[tokio::test]
    async fn encode_obeys_stop() {
        let stop = CancellationToken::new();
        let s2 = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            s2.cancel();
        });
        let r = encode(Path::new("sh"), &sh("sleep 5"), 0, Duration::from_secs(10), &stop, |_| {}).await;
        assert!(matches!(r, Err(RunError::Stopped)));
    }

    #[test]
    fn progress_lines_and_tails() {
        assert_eq!(progress_ms("out_time_us=12345678"), Some(12_345));
        assert_eq!(progress_ms("out_time_us=N/A"), None);
        assert_eq!(progress_ms("out_time_us=-9223372036854775807"), None);
        assert_eq!(progress_ms("out_time_us=-1"), None);
        assert_eq!(progress_ms("frame=10"), None);
        assert_eq!(tail("ééé", 3), "é");
        assert_eq!(tail("abc", 10), "abc");
    }

    // ---- killing, reaping, process groups --------------------------------------------------

    /// `sh -c` script that records its own pid in `file` and then becomes `sleep 30`.
    fn sleeper(file: &Path) -> Vec<String> {
        sh(&format!("echo $$ > '{}'; exec sleep 30", file.display()))
    }

    async fn read_pid(file: &Path) -> i32 {
        for _ in 0..200 {
            if let Some(pid) = std::fs::read_to_string(file).ok().and_then(|s| s.trim().parse().ok()) {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{} never got a pid", file.display());
    }

    /// Cancels `stop` once `file` holds a pid.
    fn cancel_when_pid_written(file: &Path, stop: &CancellationToken) {
        let (file, stop) = (file.to_path_buf(), stop.clone());
        tokio::spawn(async move {
            read_pid(&file).await;
            stop.cancel();
        });
    }

    /// `ps`'s state column for `pid`: empty when there is no such process.
    async fn ps_stat(pid: i32) -> String {
        let out = Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().await.unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The process is gone *right now*: not running, and not a zombie either (reaped).
    async fn assert_reaped(pid: i32) {
        assert_eq!(ps_stat(pid).await, "", "pid {pid} was not reaped before the call returned");
    }

    /// Polls `ps` for up to 2 s until the process is gone. `strict` = fully reaped (no entry at
    /// all); otherwise a zombie also counts (a reparented grandchild is reaped by init).
    async fn assert_gone(pid: i32, strict: bool) {
        let mut last = String::new();
        for _ in 0..40 {
            last = ps_stat(pid).await;
            if last.is_empty() || (!strict && last.starts_with('Z')) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("pid {pid} is still there (ps stat {last:?})");
    }

    #[tokio::test]
    async fn capture_timeout_kills_and_reaps_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pid");
        let r = capture(Path::new("sh"), &sleeper(&file), Duration::from_millis(1500), &no_stop()).await;
        assert!(matches!(r, Err(RunError::Timeout)), "{r:?}");
        assert_reaped(read_pid(&file).await).await;
    }

    #[tokio::test]
    async fn capture_stop_kills_and_reaps_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pid");
        let stop = CancellationToken::new();
        cancel_when_pid_written(&file, &stop);
        let r = capture(Path::new("sh"), &sleeper(&file), Duration::from_secs(60), &stop).await;
        assert!(matches!(r, Err(RunError::Stopped)), "{r:?}");
        assert_reaped(read_pid(&file).await).await;
    }

    #[tokio::test]
    async fn encode_timeout_kills_and_reaps_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pid");
        let r = encode(Path::new("sh"), &sleeper(&file), 0, Duration::from_millis(1500), &no_stop(), |_| {}).await;
        assert!(matches!(r, Err(RunError::Timeout)), "{r:?}");
        assert_reaped(read_pid(&file).await).await;
    }

    #[tokio::test]
    async fn encode_stop_under_nice_kills_and_reaps_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pid");
        let stop = CancellationToken::new();
        cancel_when_pid_written(&file, &stop);
        // `nice` execs the program in place, so the pid the script records is the child's.
        let r = encode(Path::new("sh"), &sleeper(&file), 5, Duration::from_secs(60), &stop, |_| {}).await;
        assert!(matches!(r, Err(RunError::Stopped)), "{r:?}");
        assert_reaped(read_pid(&file).await).await;
    }

    /// `sh` forks a `sleep` and waits for it: two processes in one group.
    fn forker(leader: &Path, grandchild: &Path) -> Vec<String> {
        sh(&format!("echo $$ > '{}'; sleep 30 & echo $! > '{}'; wait", leader.display(), grandchild.display()))
    }

    #[tokio::test]
    async fn stop_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let (leader, grandchild) = (dir.path().join("leader"), dir.path().join("grandchild"));
        let stop = CancellationToken::new();
        cancel_when_pid_written(&grandchild, &stop);
        let r = encode(Path::new("sh"), &forker(&leader, &grandchild), 0, Duration::from_secs(60), &stop, |_| {}).await;
        assert!(matches!(r, Err(RunError::Stopped)), "{r:?}");
        assert_reaped(read_pid(&leader).await).await;
        assert_gone(read_pid(&grandchild).await, false).await;
    }

    #[tokio::test]
    async fn dropping_the_future_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let (leader, grandchild) = (dir.path().join("leader"), dir.path().join("grandchild"));
        let script = forker(&leader, &grandchild);
        let stop = no_stop();
        tokio::select! {
            r = capture(Path::new("sh"), &script, Duration::from_secs(60), &stop) => panic!("finished: {r:?}"),
            _ = read_pid(&grandchild) => {} // the capture future is dropped here
        }
        assert_gone(read_pid(&leader).await, false).await;
        assert_gone(read_pid(&grandchild).await, false).await;
    }

    /// The leader has exited but a descendant still holds the pipes, so `supervise` is waiting out
    /// the grace window. A future dropped then (a worker shutdown) must not leave that descendant
    /// running: the group guard stays armed until the loop ends.
    #[tokio::test]
    async fn dropping_the_future_in_the_grace_window_kills_the_descendant_holding_the_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pid");
        let script = sh(&format!("sleep 30 & echo $! > '{}'; exit 0", file.display()));
        let stop = no_stop();
        let started = std::time::Instant::now();
        tokio::select! {
            r = encode(Path::new("sh"), &script, 0, Duration::from_secs(60), &stop, |_| {}) => panic!("finished: {r:?}"),
            _ = async {
                read_pid(&file).await;
                // The shell exited at once; this is well inside the 2 s grace.
                tokio::time::sleep(Duration::from_millis(300)).await;
            } => {} // the encode future is dropped here
        }
        assert!(started.elapsed() < REAP_GRACE, "the test must drop the future inside the grace window");
        assert_gone(read_pid(&file).await, false).await;
    }

    #[tokio::test]
    async fn a_descendant_holding_the_pipes_is_killed_after_the_child_exits() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pid");
        // The shell exits at once; its `sleep` keeps stdout and stderr open.
        let script = sh(&format!("sleep 30 & echo $! > '{}'; exit 0", file.display()));
        let t = std::time::Instant::now();
        encode(Path::new("sh"), &script, 0, Duration::from_secs(60), &no_stop(), |_| {}).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
        assert_gone(read_pid(&file).await, false).await;
    }

    // ---- bounded output --------------------------------------------------------------------

    #[tokio::test]
    async fn capture_drains_both_pipes_past_the_pipe_buffer() {
        // 200 kB on each stream, interleaved: a reader that serves one pipe at a time deadlocks.
        let script = "i=0; while [ $i -lt 400 ]; do printf '%0500d' 0; printf '%0500d' 1 >&2; i=$((i+1)); done";
        let c = capture(Path::new("sh"), &sh(script), Duration::from_secs(20), &no_stop()).await.unwrap();
        assert_eq!(c.stdout.len(), 200_000);
        assert!(c.stderr.starts_with(ELLIPSIS) && c.stderr.len() <= STDERR_TAIL, "{}", c.stderr.len());
    }

    #[tokio::test]
    async fn capture_refuses_stdout_past_the_cap() {
        let r = capture(Path::new("sh"), &sh("head -c 9000000 /dev/zero"), Duration::from_secs(30), &no_stop()).await;
        assert!(matches!(r, Err(RunError::OutputTooLarge)), "{r:?}");
        // Exactly at the cap is fine.
        let ok = capture(Path::new("sh"), &sh(&format!("head -c {CAPTURE_STDOUT_MAX} /dev/zero")), Duration::from_secs(30), &no_stop())
            .await
            .unwrap();
        assert_eq!(ok.stdout.len(), CAPTURE_STDOUT_MAX);
    }

    #[tokio::test]
    async fn encode_keeps_only_a_bounded_stderr_tail() {
        let script = "yes abcdefghij | head -c 50000000 >&2; exit 1";
        match encode(Path::new("sh"), &sh(script), 0, Duration::from_secs(60), &no_stop(), |_| {}).await {
            Err(RunError::Failed { stderr_tail, .. }) => {
                assert!(stderr_tail.starts_with(ELLIPSIS), "{stderr_tail:?}");
                assert!(stderr_tail.len() <= STDERR_TAIL, "{}", stderr_tail.len());
                assert!(stderr_tail.len() > STDERR_TAIL - 8, "{}", stderr_tail.len());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_stderr_ring_marks_truncation_and_respects_characters() {
        let mut t = Tail::new(8);
        t.push(b"abc");
        assert_eq!(t.finish(), "abc", "nothing dropped, no marker");

        let mut t = Tail::new(8);
        t.push(b"abcdefghij");
        assert_eq!(t.finish(), "…fghij");

        let mut t = Tail::new(8);
        for part in [&b"abcd"[..], b"efgh", b"ij"] {
            t.push(part);
        }
        assert_eq!(t.finish(), "…fghij");

        // 5000 bytes of two-byte characters pushed in odd-sized pieces: valid text, bounded.
        let mut t = Tail::new(STDERR_TAIL);
        for piece in "é".repeat(2500).as_bytes().chunks(7) {
            t.push(piece);
        }
        let s = t.finish();
        assert!(s.starts_with(ELLIPSIS) && s.len() <= STDERR_TAIL && !s.contains('\u{fffd}'), "{} {s:?}", s.len());
    }

    /// However the output arrives, the ring never holds more than `max` bytes.
    #[test]
    fn the_stderr_ring_never_exceeds_its_bound() {
        // (3 is the least `finish` can mark with its ellipsis; the real bound is STDERR_TAIL.)
        for max in [3, 4, 7, 8, 64, STDERR_TAIL] {
            let mut t = Tail::new(max);
            // Chunks from empty to three times the bound, in a fixed pseudo-random order.
            let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
            for i in 0..2000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let len = (x % (3 * max as u64 + 1)) as usize;
                t.push(&vec![b'a' + (i % 26) as u8; len]);
                assert!(t.buf.len() <= t.max, "max {max}: {} bytes after push {i} of {len}", t.buf.len());
            }
            let s = t.finish();
            assert!(s.len() <= max, "max {max}: finish() gave {} bytes", s.len());
        }
    }

    #[tokio::test]
    async fn a_stray_byte_on_stdout_does_not_stop_the_progress_drain() {
        let mut seen = Vec::new();
        // Invalid UTF-8, then a line over the limit, then real progress.
        let script = "printf '\\377\\376junk\\n'; printf 'out_time_us='; head -c 10000 /dev/zero | tr '\\0' 1; echo; printf 'out_time_us=3000000\\n'";
        encode(Path::new("sh"), &sh(script), 0, Duration::from_secs(10), &no_stop(), |ms| seen.push(ms)).await.unwrap();
        assert_eq!(seen, vec![3000]);
    }

    #[tokio::test]
    async fn encode_runs_the_child_at_the_requested_niceness() {
        // `nice` with no arguments errors on macOS, so the script reads its niceness from ps and
        // reports it through the progress channel (out_time_us=N000 reads back as N ms).
        let script = sh("printf 'out_time_us=%s000\\n' \"$(ps -o ni= -p $$ | tr -d ' ')\"");
        let mut base = Vec::new();
        encode(Path::new("sh"), &script, 0, Duration::from_secs(10), &no_stop(), |n| base.push(n)).await.unwrap();
        let mut niced = Vec::new();
        encode(Path::new("sh"), &script, 5, Duration::from_secs(10), &no_stop(), |n| niced.push(n)).await.unwrap();
        assert_eq!((base.len(), niced.len()), (1, 1));
        assert_eq!(niced[0], (base[0] + 5).min(19), "base {base:?}");
    }

    #[tokio::test]
    async fn the_child_does_not_inherit_the_workers_environment() {
        // The test process has plenty of variables (cargo sets several); `env` must print only
        // what `command` passes on.
        let c = capture(Path::new("env"), &[], Duration::from_secs(5), &no_stop()).await.unwrap();
        let names: Vec<&str> = c.stdout.lines().filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
        assert!(names.contains(&"PATH"), "{names:?}");
        assert!(names.iter().all(|k| ["PATH", "LANG", "LC_ALL"].contains(k)), "{names:?}");
    }
}
