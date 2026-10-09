//! Operator test boot (spec §6.3) — the parts that need no database.
//!
//! A test boot rents one GPU in a provider and zone the operator pins, boots the provider's
//! plain GPU image with a probe in its cloud-init, waits for the probe's report, destroys the
//! machine and confirms it is gone. The probe reports the GPU and whether NVENC can encode one
//! second of video, to mm-core's public boot-report endpoint, with a single-use token whose
//! hash is all the database keeps.

use chrono::{DateTime, Utc};
use mm_core::fleet::NodeId;
use rand::TryRngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The hard deadline from the request (spec §6.3, a constant, not a setting).
pub const DEADLINE_SECS: i64 = 900;
/// How long the runner waits for a report before it destroys the machine anyway.
pub const BOOT_WAIT_SECS: i64 = 600;
/// Where the probe reports: mm-core's client listener, under a prefix the edge routes.
pub const REPORT_PATH: &str = "/_mm/webhooks/fleet/boot-report";
pub const MAX_REPORT_BYTES: usize = 4096;
/// A probe's uptime and its own runtime are each at most a day: a longer one is not a test boot.
const MAX_REPORT_SECS: u32 = 86_400;

/// A test boot's node id: the request's id with `tb-` for `r-`, so each finds the other.
/// Only ever called with an `r-` id, the request ids the queue mints.
pub fn node_id_for(request_id: &str) -> NodeId {
    debug_assert!(
        request_id.starts_with("r-"),
        "a test-boot request id starts with r-"
    );
    NodeId::new(format!(
        "tb-{}",
        request_id.strip_prefix("r-").unwrap_or(request_id)
    ))
}

/// The request id a test-boot node belongs to. `None` for any other node id, and for a bare
/// `tb-` with nothing after it.
pub fn request_id_for(node_id: &str) -> Option<String> {
    node_id
        .strip_prefix("tb-")
        .filter(|s| !s.is_empty())
        .map(|s| format!("r-{s}"))
}

/// The probe's report URL from `server.public_url`: an https origin, optionally with a path
/// prefix. The probe sends its token in the `Authorization` header, never in this URL, so https
/// is what protects it in transit. A query, fragment or userinfo is refused: the report path is
/// appended to the string, so one of those would make the probe post to a different URL, or put
/// a credential into the URL.
pub fn report_url(public_url: &str) -> Result<String, &'static str> {
    let base = public_url.trim().trim_end_matches('/');
    if base
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '\\')
    {
        return Err("server.public_url contains characters a URL cannot");
    }
    let parsed = url::Url::parse(base).map_err(|_| "server.public_url is not an absolute URL")?;
    if parsed.scheme() != "https" {
        return Err("server.public_url must use https: the test machine reports over the internet");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("server.public_url must not carry userinfo (user:password@)");
    }
    if parsed.query().is_some() {
        return Err("server.public_url must not carry a query (?...)");
    }
    if parsed.fragment().is_some() {
        return Err("server.public_url must not carry a fragment (#...)");
    }
    // The parsed form, so the origin is normalised before the path is appended.
    Ok(format!(
        "{}{REPORT_PATH}",
        parsed.as_str().trim_end_matches('/')
    ))
}

/// A fresh token (64 lowercase hex characters) and its SHA-256, which is all that is stored.
/// The bytes come straight from the OS CSPRNG (`OsRng`), not a user-space generator. If the OS
/// source fails, this panics rather than mint a weak token.
pub fn mint_token() -> (String, Vec<u8>) {
    let mut bytes = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .expect("the OS random source is unavailable");
    let token = hex::encode(bytes);
    let hash = token_hash(&token);
    (token, hash)
}

pub fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// A cheap shape check before any lookup: 64 lowercase hex characters.
pub fn looks_like_token(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What the probe posts. Strict: an unknown field is a 400, not silently kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootReport {
    pub v: u32,
    /// `nvidia-smi` name and driver version, or its error.
    pub gpu: String,
    /// `ok` or `fail`.
    pub nvenc: String,
    #[serde(default)]
    pub nvenc_error: Option<String>,
    pub uptime_secs: u32,
    pub probe_secs: u32,
}

impl BootReport {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.v != 1 {
            return Err("v must be 1");
        }
        if self.gpu.chars().count() > 200 {
            return Err("gpu is at most 200 characters");
        }
        if !plain_text(&self.gpu) {
            return Err("gpu must not hold control characters");
        }
        if self.nvenc != "ok" && self.nvenc != "fail" {
            return Err("nvenc must be ok or fail");
        }
        if self.nvenc == "ok" && self.nvenc_error.is_some() {
            return Err("nvenc is ok, so nvenc_error must be null");
        }
        if self
            .nvenc_error
            .as_deref()
            .is_some_and(|e| e.chars().count() > 400)
        {
            return Err("nvenc_error is at most 400 characters");
        }
        if !self.nvenc_error.as_deref().is_none_or(plain_text) {
            return Err("nvenc_error must not hold control characters");
        }
        if self.uptime_secs > MAX_REPORT_SECS {
            return Err("uptime_secs is at most 86400");
        }
        if self.probe_secs > MAX_REPORT_SECS {
            return Err("probe_secs is at most 86400");
        }
        Ok(())
    }
}

/// Text with no control character but a newline or a tab. A NUL in particular cannot be stored:
/// Postgres refuses `\u0000` in JSONB, so a report holding one would fail as a server error on
/// every retry instead of being refused as malformed.
fn plain_text(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
}

/// The node row's `boot_report`: the report as received, and when.
pub fn stored_report(r: &BootReport, received_at: DateTime<Utc>) -> Value {
    json!({ "report": r, "received_at": received_at })
}

/// The stored report, if it is still valid. A row that does not deserialize, or that fails
/// `validate` (written without it, or by an older shape), reads as absent rather than as data.
pub fn report_of(stored: &Value) -> Option<BootReport> {
    serde_json::from_value::<BootReport>(stored.get("report")?.clone())
        .ok()
        .filter(|r| r.validate().is_ok())
}

/// Whole minutes billed between two instants: rounded up from milliseconds, at least one.
pub fn billed_minutes(started: DateTime<Utc>, ended: DateTime<Utc>) -> i64 {
    let millis = (ended - started).num_milliseconds().max(0);
    ((millis + 59_999) / 60_000).max(1)
}

/// List price × minutes, rounded up to the cent. `None` when the price is unknown, not finite,
/// or negative.
///
/// The `- 1e-9` is a float guard: `1.12 / 60 * 15 * 100` is `28.000000000000004`, and a bare
/// `ceil` would bill a cent more than the true `0.28`. The dashboard uses the same guard, so
/// the page's "at most" figure and the server's figure agree.
pub fn estimate_cost(price_per_hour: Option<f64>, minutes: i64) -> Option<f64> {
    price_per_hour
        .filter(|p| p.is_finite() && *p >= 0.0)
        .map(|p| (p * minutes as f64 / 60.0 * 100.0 - 1e-9).ceil().max(0.0) / 100.0)
}

const PROBE: &str = r#"#!/usr/bin/env python3
# Test boot probe: report the GPU and whether NVENC encodes, then exit.
import json, subprocess, time, urllib.request
t0 = time.monotonic()
env = {}
for line in open('/etc/mm-boot-probe.env'):
    if '=' in line:
        k, v = line.strip().split('=', 1)
        env[k] = v
def run(cmd, timeout):
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return p.returncode, (p.stdout + p.stderr).strip()
    except Exception as e:
        return 1, str(e)
_, gpu = run(['nvidia-smi', '--query-gpu=name,driver_version', '--format=csv,noheader'], 60)
code, _ = run(['sh', '-c', 'command -v ffmpeg || (apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ffmpeg)'], 420)
nvenc, err = 'fail', None
if code == 0:
    code, out = run(['ffmpeg', '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=1280x720:rate=30', '-t', '1', '-c:v', 'h264_nvenc', '-f', 'null', '-'], 120)
    if code == 0:
        nvenc = 'ok'
    else:
        err = out[-400:]
else:
    err = 'ffmpeg could not be installed'
uptime = int(float(open('/proc/uptime').read().split()[0]))
body = json.dumps({'v': 1, 'gpu': gpu[:200], 'nvenc': nvenc, 'nvenc_error': err, 'uptime_secs': uptime, 'probe_secs': int(time.monotonic() - t0)}, ensure_ascii=False).encode('utf-8')
req = urllib.request.Request(env['MM_REPORT_URL'], data=body, method='POST', headers={'Authorization': 'Bearer ' + env['MM_REPORT_TOKEN'], 'Content-Type': 'application/json'})
class NoRedirect(urllib.request.HTTPRedirectHandler):
    # urllib copies the Authorization header onto a redirect target, even on another host, so
    # the token must never follow one: a redirect is refused and the request fails.
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None
opener = urllib.request.build_opener(NoRedirect)
for attempt in range(5):
    try:
        opener.open(req, timeout=20)
        break
    except Exception:
        time.sleep(10)
"#;

fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{pad}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cloud-init for a test boot. `report_url` comes from `report_url()` and `token` from
/// `mint_token()`, so neither can carry a newline or a quote into the YAML.
///
/// The returned text contains the token. It must never be logged, persisted, or put in an
/// error: it goes to the provider as user data and nowhere else.
///
/// Debug builds check the two inputs' shape. `report_url()` and `mint_token()` guarantee it in
/// every build, so a release build cannot receive a value that breaks the YAML.
pub fn probe_cloud_init(report_url: &str, token: &str) -> String {
    debug_assert!(
        looks_like_token(token),
        "the probe token must be a minted token"
    );
    debug_assert!(
        !report_url.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\')),
        "the report URL must come from report_url()"
    );
    format!(
        "#cloud-config\n\
         write_files:\n\
         \x20 - path: /etc/mm-boot-probe.env\n\
         \x20   permissions: \"0600\"\n\
         \x20   content: |\n\
         \x20     MM_REPORT_URL={report_url}\n\
         \x20     MM_REPORT_TOKEN={token}\n\
         \x20 - path: /usr/local/sbin/mm-boot-probe\n\
         \x20   permissions: \"0700\"\n\
         \x20   content: |\n\
         {script}\n\
         runcmd:\n\
         \x20 - [ /usr/local/sbin/mm-boot-probe ]\n",
        script = indent(PROBE, 6)
    )
}

/// Cloud-init for a broadcast transcoder. What the transcode software needs belongs to WS-G;
/// until then it names the node and its flavor. The id is reduced to `[A-Za-z0-9._-]` so it
/// cannot inject YAML.
pub fn transcode_cloud_init(mm_node_id: &NodeId) -> String {
    let id: String = mm_node_id
        .as_str()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    format!(
        "#cloud-config\n\
         write_files:\n\
         \x20 - path: /etc/mm-transcode.env\n\
         \x20   permissions: \"0600\"\n\
         \x20   content: |\n\
         \x20     MM_NODE_ID={id}\n\
         \x20     MM_NODE_FLAVOR=transcode\n"
    )
}
