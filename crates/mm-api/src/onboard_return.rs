//! Browser landing pages for the end of Stripe Connect onboarding.
//!
//! `POST /creator/onboard` hands the app a Stripe-hosted onboarding URL (an
//! account link). Stripe later sends the creator's browser to one of the link's
//! two URLs:
//!
//! - `return_url` when the creator leaves the flow. That is not proof onboarding
//!   is finished: they may have closed it halfway. The `account.updated` webhook
//!   (and so `GET /creator/profile`) is what says the account can take payments.
//! - `refresh_url` when the link has expired or was already used. Stripe expects
//!   a new link here, but creating one needs the creator's token, which a browser
//!   redirect does not carry; the app asks for a new one with `POST
//!   /creator/onboard`.
//!
//! So a page only tells the creator to go back to the app, and changes nothing.
//!
//! The pages live under `/_mm/client/v1` because that is the one prefix that
//! reaches mm-core in every deployment: the compose stack's Traefik sends any
//! path outside `/_mm`, `/mm/v1`, `/livekit` and `/lk-jwt` to Synapse, and the
//! Helm ingress sends everything to mm-core. The old
//! `{public_url}/creator/onboard/return` was answered by Synapse with a 404.

use axum::{
    Router,
    http::header,
    response::{Html, IntoResponse},
    routing::get,
};

/// Where the pages are served, next to the `POST /creator/onboard` that issues
/// the link. The URLs handed to Stripe are built from it too, so they and the
/// routes below cannot drift apart.
const BASE_PATH: &str = "/_mm/client/v1/creator/onboard";

/// The account link's `return_url`; `public_url` is `server.public_url`.
pub fn return_url(public_url: &str) -> String {
    page_url(public_url, "return")
}

/// The account link's `refresh_url`; `public_url` is `server.public_url`.
pub fn refresh_url(public_url: &str) -> String {
    page_url(public_url, "refresh")
}

fn page_url(public_url: &str, outcome: &str) -> String {
    format!("{}{BASE_PATH}/{outcome}", public_url.trim_end_matches('/'))
}

/// The landing pages, already mounted at [`BASE_PATH`].
///
/// Stateless on purpose: without `SharedState` a page has no way to read or
/// write a creator profile or a connected account.
pub fn router() -> Router {
    Router::new().nest(
        BASE_PATH,
        Router::new()
            .route("/return", get(returned))
            .route("/refresh", get(expired)),
    )
}

async fn returned() -> impl IntoResponse {
    page(
        "Back from Stripe",
        "You can close this page and return to the app. It shows whether Stripe still needs anything from you, and can take a few seconds to update.",
    )
}

async fn expired() -> impl IntoResponse {
    page(
        "This link has expired",
        "Stripe onboarding links work only once and expire after a few minutes. Close this page, return to the app and start creator onboarding again for a new link.",
    )
}

fn page(title: &str, message: &str) -> impl IntoResponse {
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            // Served from the API origin: allow nothing but the inline style.
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'",
            ),
        ],
        Html(format!(
            r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>{title} · MatrixMedia</title>
<style>
:root {{ color-scheme: light dark; }}
body {{ margin: 0; min-height: 100vh; box-sizing: border-box; padding: 24px; display: grid; place-items: center; text-align: center; font: 17px/1.5 system-ui, -apple-system, sans-serif; }}
h1 {{ margin: 0 0 8px; font-size: 1.5rem; }}
p {{ margin: 0; opacity: 0.75; }}
</style>
</head>
<body>
<main>
<h1>{title}</h1>
<p>{message}</p>
</main>
</body>
</html>
"#
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    const PUBLIC_URL: &str = "https://matrix.example.org";

    async fn get_page(path: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        let resp = router()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    /// Fetches the page Stripe would send the browser to, after checking the
    /// edge proxy would route it to mm-core at all.
    async fn page_for(url: &str) -> String {
        let path = url.strip_prefix(PUBLIC_URL).unwrap();
        // The edge proxy only sends /_mm/client/** (and a few others) to
        // mm-core; everything else on this host goes to Synapse.
        assert!(
            path.starts_with("/_mm/client/"),
            "{url} would not reach mm-core"
        );
        let (status, headers, body) = get_page(path).await;
        assert_eq!(status, StatusCode::OK, "{url}");
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        body
    }

    #[tokio::test]
    async fn the_return_url_is_served_and_sends_the_creator_back_to_the_app() {
        let body = page_for(&return_url(PUBLIC_URL)).await;
        // Leaving Stripe is not finishing onboarding: the page must not claim
        // the account is ready.
        assert!(body.contains("<h1>Back from Stripe</h1>"), "{body}");
        assert!(body.contains("return to the app"), "{body}");
        assert!(!body.contains("complete"), "{body}");
    }

    #[tokio::test]
    async fn the_refresh_url_is_served_and_says_to_start_again_from_the_app() {
        let body = page_for(&refresh_url(PUBLIC_URL)).await;
        assert!(body.contains("<h1>This link has expired</h1>"), "{body}");
        assert!(body.contains("return to the app"), "{body}");
        assert!(body.contains("start creator onboarding again"), "{body}");
    }

    #[test]
    fn a_trailing_slash_on_public_url_is_not_doubled() {
        assert_eq!(
            return_url("https://matrix.example.org/"),
            "https://matrix.example.org/_mm/client/v1/creator/onboard/return",
        );
        assert_eq!(
            refresh_url("https://matrix.example.org/"),
            "https://matrix.example.org/_mm/client/v1/creator/onboard/refresh",
        );
    }

    #[tokio::test]
    async fn only_the_two_outcomes_are_pages() {
        let (status, _, _) = get_page(&format!("{BASE_PATH}/complete")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // The old, unprefixed path is not served either: Synapse owns it.
        let (status, _, _) = get_page("/creator/onboard/return").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_pages_are_read_only() {
        for path in [return_url(""), refresh_url("")] {
            let resp = router()
                .oneshot(Request::post(&path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED, "{path}");
        }
    }
}
