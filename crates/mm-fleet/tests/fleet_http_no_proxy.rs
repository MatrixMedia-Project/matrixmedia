//! `fleet_http()` must ignore the proxy environment variables (HTTP_PROXY, HTTPS_PROXY, ALL_PROXY).
//!
//! A proxy resolves the target host itself, so with one configured the request would bypass
//! `GuardedResolver` and a private or metadata address would be reachable, with the token header
//! on the request. The test must not change this process's environment (other tests share it),
//! so it re-runs itself as a child process that has the proxy variables set, pointing at a
//! listener the parent owns; the parent then checks that nothing connected to it.

use std::net::TcpListener;
use std::process::Command;
use std::time::Duration;

const CHILD_MARKER: &str = "MM_TEST_FLEET_HTTP_PROXY_CHILD";
const TEST_NAME: &str = "proxy_environment_variables_do_not_bypass_the_resolver_guard";

#[test]
fn proxy_environment_variables_do_not_bypass_the_resolver_guard() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        child();
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a stand-in proxy");
    listener.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());

    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD_MARKER, "1")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
    for var in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env(var, &proxy);
    }
    let out = cmd.output().expect("run the child");
    assert!(
        out.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // A connection to the stand-in proxy completes at the TCP level even if nobody accepts it,
    // so a client that honoured the variable leaves one pending here.
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("fleet_http() connected to the proxy named in the environment"),
        Err(e) => panic!("accept: {e}"),
    }
}

/// Runs in the child: one request to a name that cannot resolve. Without a proxy the guarded
/// resolver rejects it at once; with one the client would dial the proxy instead.
fn child() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let call = mm_fleet::endpoint::fleet_http()
            .get("http://provider.invalid/")
            .send();
        // The stand-in proxy never answers; do not wait for the client's own 60 s deadline.
        let _ = tokio::time::timeout(Duration::from_secs(3), call).await;
    });
}
