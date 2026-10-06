//! Browser landing pages for the end of a Stripe Checkout.
//!
//! Stripe sends the viewer's browser (an Android Custom Tab, iOS Safari) to the
//! session's `success_url` or `cancel_url` when checkout ends. The apps never
//! read these pages: they open checkout, wait for the viewer to come back, and
//! re-check entitlement. So a page only tells the viewer to return to the app,
//! and changes nothing — the `checkout.session.completed` webhook is what
//! settles a donation or subscription.
//!
//! The pages live under `/_mm/client/v1` because that is the one prefix that
//! reaches mm-core in every deployment: the compose stack's Traefik sends any
//! path outside `/_mm`, `/mm/v1`, `/livekit` and `/lk-jwt` to Synapse, and the
//! Helm ingress sends everything to mm-core. The old
//! `{public_url}/subscriptions/{id}/success` was answered by Synapse with a 404.

use axum::{
    Router,
    http::header,
    response::{Html, IntoResponse},
    routing::get,
};
use uuid::Uuid;

/// Where the pages are served. The URLs handed to Stripe are built from it too,
/// so they and the routes below cannot drift apart.
const BASE_PATH: &str = "/_mm/client/v1/checkout";

/// What a checkout session pays for.
#[derive(Debug, Clone, Copy)]
pub enum CheckoutKind {
    Donation,
    Subscription,
}

impl CheckoutKind {
    fn segment(self) -> &'static str {
        match self {
            Self::Donation => "donations",
            Self::Subscription => "subscriptions",
        }
    }
}

/// The session's `success_url`; `public_url` is `server.public_url`.
pub fn success_url(public_url: &str, kind: CheckoutKind, id: Uuid) -> String {
    page_url(public_url, kind, id, "success")
}

/// The session's `cancel_url`; `public_url` is `server.public_url`.
pub fn cancel_url(public_url: &str, kind: CheckoutKind, id: Uuid) -> String {
    page_url(public_url, kind, id, "cancel")
}

fn page_url(public_url: &str, kind: CheckoutKind, id: Uuid, outcome: &str) -> String {
    format!(
        "{}{BASE_PATH}/{}/{id}/{outcome}",
        public_url.trim_end_matches('/'),
        kind.segment()
    )
}

/// The landing pages, already mounted at [`BASE_PATH`].
///
/// Stateless on purpose: without `SharedState` a page has no way to read or
/// write a donation or subscription. The id in the path is there for the access
/// log only; the handlers never look at it, so the pages cannot be used to probe
/// which ids exist.
pub fn router() -> Router {
    Router::new().nest(
        BASE_PATH,
        Router::new()
            .route("/donations/{id}/success", get(success))
            .route("/donations/{id}/cancel", get(cancel))
            .route("/subscriptions/{id}/success", get(success))
            .route("/subscriptions/{id}/cancel", get(cancel)),
    )
}

async fn success() -> impl IntoResponse {
    page(
        "Payment received",
        "You can close this page and return to the app. The app can take a few seconds to update.",
    )
}

async fn cancel() -> impl IntoResponse {
    page(
        "Checkout cancelled",
        "You can close this page and return to the app.",
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

    #[tokio::test]
    async fn every_url_handed_to_stripe_is_served() {
        let id = Uuid::new_v4();
        for kind in [CheckoutKind::Donation, CheckoutKind::Subscription] {
            for (url, heading) in [
                (success_url(PUBLIC_URL, kind, id), "Payment received"),
                (cancel_url(PUBLIC_URL, kind, id), "Checkout cancelled"),
            ] {
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
                assert!(
                    body.contains(&format!("<h1>{heading}</h1>")),
                    "{url}: {body}"
                );
                assert!(body.contains("return to the app"), "{url}: {body}");
            }
        }
    }

    #[test]
    fn a_trailing_slash_on_public_url_is_not_doubled() {
        let id = Uuid::nil();
        assert_eq!(
            success_url(
                "https://matrix.example.org/",
                CheckoutKind::Subscription,
                id
            ),
            format!("https://matrix.example.org/_mm/client/v1/checkout/subscriptions/{id}/success"),
        );
    }

    #[tokio::test]
    async fn only_the_two_outcomes_are_pages() {
        let id = Uuid::new_v4();
        let (status, _, _) = get_page(&format!("{BASE_PATH}/subscriptions/{id}/refund")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = get_page(&format!("{BASE_PATH}/tiers/{id}/success")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_pages_are_read_only() {
        let path = success_url("", CheckoutKind::Subscription, Uuid::new_v4());
        let resp = router()
            .oneshot(Request::post(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
