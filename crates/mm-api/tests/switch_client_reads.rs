//! SwitchClient's read calls against a stub mm-switch (axum on 127.0.0.1:0).

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use serde_json::json;

use mm_core::switch_client::SwitchClient;

async fn stub(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn health_detail_parses_the_switch_body() {
    let url = stub(Router::new().route(
        "/health",
        get(|| async {
            axum::Json(
                json!({"status": "ok", "sources": 2, "viewers": 37, "recorders": {"recording": 1}}),
            )
        }),
    ))
    .await;
    let h = SwitchClient::new(&url).health_detail().await.unwrap();
    assert_eq!(h.sources, 2);
    assert_eq!(h.viewers, 37);
    assert_eq!(h.recorders.get("recording"), Some(&1));
}

#[tokio::test]
async fn health_detail_reads_null_or_missing_recorders_as_empty() {
    // Test case 1: recorders explicitly null (Go's empty map serialization)
    let url_null = stub(Router::new().route(
        "/health",
        get(|| async {
            axum::Json(json!({"status": "ok", "sources": 0, "viewers": 0, "recorders": null}))
        }),
    ))
    .await;
    assert!(
        SwitchClient::new(&url_null)
            .health_detail()
            .await
            .unwrap()
            .recorders
            .is_empty()
    );

    // Test case 2: recorders field completely absent
    let url_missing = stub(Router::new().route(
        "/health",
        get(|| async { axum::Json(json!({"status": "ok", "sources": 0, "viewers": 0})) }),
    ))
    .await;
    assert!(
        SwitchClient::new(&url_missing)
            .health_detail()
            .await
            .unwrap()
            .recorders
            .is_empty()
    );
}

#[tokio::test]
async fn health_detail_is_an_error_on_non_2xx() {
    let url =
        stub(Router::new().route("/health", get(|| async { StatusCode::SERVICE_UNAVAILABLE })))
            .await;
    let err = SwitchClient::new(&url).health_detail().await.unwrap_err();
    assert!(err.contains("503"), "{err}");
}

#[tokio::test]
async fn health_detail_is_an_error_when_nothing_listens() {
    let err = SwitchClient::new("http://127.0.0.1:1")
        .health_detail()
        .await
        .unwrap_err();
    assert!(err.contains("switch health failed"), "{err}");
}

#[tokio::test]
async fn health_detail_is_an_error_when_the_counts_are_missing() {
    let url = stub(Router::new().route(
        "/health",
        get(|| async { axum::Json(json!({"status": "ok"})) }),
    ))
    .await;
    let err = SwitchClient::new(&url).health_detail().await.unwrap_err();
    assert!(err.contains("switch /health body"), "{err}");
}

#[tokio::test]
async fn a_401_viewer_list_is_an_error_not_an_empty_list() {
    // The bug this replaces: a 401 body has no `viewers`, and the old code read that as
    // "nobody is watching".
    let url = stub(Router::new().route(
        "/api/viewers",
        get(|| async {
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "unauthorized"})),
            )
        }),
    ))
    .await;
    let err = SwitchClient::new(&url).list_viewers().await.unwrap_err();
    assert!(err.contains("401"), "{err}");
}

#[tokio::test]
async fn a_body_without_the_list_is_an_error() {
    let url = stub(Router::new().route(
        "/api/sources",
        get(|| async { axum::Json(json!({"other": []})) }),
    ))
    .await;
    let err = SwitchClient::new(&url).list_sources().await.unwrap_err();
    assert!(err.contains("no `sources`"), "{err}");
}

#[tokio::test]
async fn a_null_list_is_empty() {
    let url = stub(Router::new().route(
        "/api/viewers",
        get(|| async { axum::Json(json!({"viewers": null})) }),
    ))
    .await;
    assert!(
        SwitchClient::new(&url)
            .list_viewers()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn lists_parse_and_carry_the_server_token() {
    let url = stub(
        Router::new()
            .route(
                "/api/sources",
                get(|headers: HeaderMap| async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.starts_with("Bearer "));
                    if !bearer {
                        return (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
                    }
                    (
                        StatusCode::OK,
                        axum::Json(json!({"sources": [{"id": "stream-a", "type": "webrtc", "active": true}]})),
                    )
                }),
            )
            .route(
                "/api/viewers",
                get(|| async {
                    axum::Json(json!({"viewers": [
                        {"id": "viewer-a--u-example.org", "current_source": "stream-a", "connected": true}
                    ]}))
                }),
            ),
    )
    .await;
    let client = SwitchClient::with_auth(&url, "switch-secret-for-tests-only".into());
    let sources = client.list_sources().await.unwrap();
    assert_eq!(sources.len(), 1);
    assert!(sources[0].active);
    let viewers = client.list_viewers().await.unwrap();
    assert_eq!(viewers[0].current_source, "stream-a");
}
