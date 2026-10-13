//! Running ffmpeg and ffprobe. Every child is killed when its future is dropped
//! (`kill_on_drop`), so a timeout, a stop request or a worker shutdown never leaves an encoder
//! behind.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub enum RunError {
    Spawn(std::io::Error),
    Timeout,
    Stopped,
    Failed { status: String, stderr_tail: String },
}

#[derive(Debug)]
pub struct Captured {
    pub stdout: String,
    pub stderr: String,
}

const STDERR_TAIL: usize = 2048;

/// A short command (ffprobe, a loudness pass) run to completion.
pub async fn capture(program: &Path, args: &[String], timeout: Duration, stop: &CancellationToken) -> Result<Captured, RunError> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(RunError::Spawn)?;
    let out = tokio::select! {
        r = child.wait_with_output() => r.map_err(RunError::Spawn)?,
        _ = tokio::time::sleep(timeout) => return Err(RunError::Timeout),
        _ = stop.cancelled() => return Err(RunError::Stopped),
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(RunError::Failed { status: out.status.to_string(), stderr_tail: tail(&stderr, STDERR_TAIL) });
    }
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
        let mut c = Command::new("nice");
        c.arg("-n").arg(nice.to_string()).arg(program);
        c
    } else {
        Command::new(program)
    };
    let mut child = cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(RunError::Spawn)?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut lines = BufReader::new(stdout).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(l)) => {
                    if let Some(ms) = progress_ms(&l) {
                        on_progress(ms);
                    }
                }
                Ok(None) | Err(_) => break,
            },
            _ = &mut deadline => return Err(RunError::Timeout),
            _ = stop.cancelled() => return Err(RunError::Stopped),
        }
    }
    let status = tokio::select! {
        s = child.wait() => s.map_err(RunError::Spawn)?,
        _ = &mut deadline => return Err(RunError::Timeout),
        _ = stop.cancelled() => return Err(RunError::Stopped),
    };
    let stderr = stderr_task.await.unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        Err(RunError::Failed { status: status.to_string(), stderr_tail: tail(&stderr, STDERR_TAIL) })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["-c".into(), script.into()]
    }

    #[tokio::test]
    async fn capture_returns_both_streams() {
        let c = capture(Path::new("sh"), &sh("echo out; echo err >&2"), Duration::from_secs(5), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!((c.stdout.trim(), c.stderr.trim()), ("out", "err"));
    }

    #[tokio::test]
    async fn a_failure_keeps_the_stderr_tail() {
        match capture(Path::new("sh"), &sh("echo boom >&2; exit 3"), Duration::from_secs(5), &CancellationToken::new()).await {
            Err(RunError::Failed { stderr_tail, .. }) => assert!(stderr_tail.contains("boom")),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn capture_times_out() {
        let t = std::time::Instant::now();
        let r = capture(Path::new("sh"), &sh("sleep 5"), Duration::from_millis(100), &CancellationToken::new()).await;
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
            &CancellationToken::new(),
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
        assert_eq!(progress_ms("frame=10"), None);
        assert_eq!(tail("ééé", 3), "é");
        assert_eq!(tail("abc", 10), "abc");
    }
}
