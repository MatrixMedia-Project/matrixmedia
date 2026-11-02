//! Federated OpenID validation must ignore the proxy environment variables (HTTP_PROXY,
//! HTTPS_PROXY, ALL_PROXY).
//!
//! A proxy resolves the target host itself, so with one configured both the well-known fetch
//! and the userinfo request would bypass the SSRF vetting and the address pinning. The test
//! must not change this process's environment (other tests share it), so it re-runs itself as
//! a child process that has the proxy variables set, pointing at a listener the parent owns;
//! the parent then checks that nothing connected to it.

use std::net::TcpListener;
use std::process::Command;

const CHILD_MARKER: &str = "MM_TEST_FEDERATION_PROXY_CHILD";
const TEST_NAME: &str = "proxy_environment_variables_do_not_bypass_federation_vetting";

#[test]
fn proxy_environment_variables_do_not_bypass_federation_vetting() {
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
        Ok(_) => panic!("federated validation connected to the proxy named in the environment"),
        Err(e) => panic!("accept: {e}"),
    }
}

/// Runs in the child, through the production resolver:
/// - `example.invalid` cannot resolve, so the vetting resolver fails the well-known fetch at
///   once; through a proxy it would be sent to the proxy instead.
/// - `198.51.100.1:8448` (TEST-NET-2) passes vetting and the userinfo request is pinned to
///   it; through a proxy it would go to the proxy.
fn child() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let client = mm_matrix::client::HomeserverClient::new(
            "http://localhost:8008".to_string(),
            "token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        for server_name in ["example.invalid", "198.51.100.1:8448"] {
            // Both fail (nothing answers); only where they connect matters.
            let _ = client
                .validate_openid_federated("token", server_name, 2)
                .await;
        }
    });
}
