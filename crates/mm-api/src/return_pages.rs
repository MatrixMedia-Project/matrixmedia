//! Browser landing pages for the end of a Stripe-hosted flow.
//!
//! When a hosted flow ends, Stripe sends the browser (an Android Custom Tab,
//! iOS Safari) to a URL we gave it: a Checkout session's `success_url` /
//! `cancel_url`, or a Connect account link's `return_url` / `refresh_url`. The
//! apps never read these pages: they open the flow, wait for the user to come
//! back, and re-read state. So a page only tells the user to return to the app,
//! and changes nothing — webhooks settle the outcome (`checkout.session.completed`
//! for a payment, `account.updated` for onboarding).
//!
//! The pages live under `/_mm/client/v1` because that is the one prefix that
//! reaches mm-core in every deployment: the compose stack's Traefik sends any
//! path outside `/_mm`, `/mm/v1`, `/livekit` and `/lk-jwt` to Synapse, and the
//! Helm ingress sends everything to mm-core. The old
//! `{public_url}/subscriptions/{id}/success` and
//! `{public_url}/creator/onboard/return` were answered by Synapse with a 404.

use axum::{
    Router,
    http::header,
    response::{Html, IntoResponse},
    routing::get,
};
use uuid::Uuid;

/// Where the pages are served. The URLs handed to Stripe are built from these
/// too, so they and the routes below cannot drift apart.
const CHECKOUT_PATH: &str = "/_mm/client/v1/checkout";
const ONBOARDING_PATH: &str = "/_mm/client/v1/onboarding";

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

/// The checkout session's `success_url`; `public_url` is `server.public_url`.
pub fn checkout_success_url(public_url: &str, kind: CheckoutKind, id: Uuid) -> String {
    checkout_url(public_url, kind, id, "success")
}

/// The checkout session's `cancel_url`; `public_url` is `server.public_url`.
pub fn checkout_cancel_url(public_url: &str, kind: CheckoutKind, id: Uuid) -> String {
    checkout_url(public_url, kind, id, "cancel")
}

fn checkout_url(public_url: &str, kind: CheckoutKind, id: Uuid, outcome: &str) -> String {
    format!(
        "{}{CHECKOUT_PATH}/{}/{id}/{outcome}",
        public_url.trim_end_matches('/'),
        kind.segment()
    )
}

/// The account link's `return_url`: where Stripe sends the creator on leaving
/// onboarding, finished or not.
pub fn onboarding_return_url(public_url: &str) -> String {
    format!(
        "{}{ONBOARDING_PATH}/return",
        public_url.trim_end_matches('/')
    )
}

/// The account link's `refresh_url`: where Stripe sends the creator when the
/// link has expired or was already used.
pub fn onboarding_refresh_url(public_url: &str) -> String {
    format!(
        "{}{ONBOARDING_PATH}/refresh",
        public_url.trim_end_matches('/')
    )
}

/// The landing pages, already mounted at their full paths.
///
/// Stateless on purpose: without `SharedState` a page has no way to read or
/// write a payment or a creator profile. The id in a checkout path is there for
/// the access log only; the handlers never look at it, so the pages cannot be
/// used to probe which ids exist.
pub fn router() -> Router {
    Router::new()
        .nest(
            CHECKOUT_PATH,
            Router::new()
                .route("/donations/{id}/success", get(checkout_success))
                .route("/donations/{id}/cancel", get(checkout_cancel))
                .route("/subscriptions/{id}/success", get(checkout_success))
                .route("/subscriptions/{id}/cancel", get(checkout_cancel)),
        )
        .nest(
            ONBOARDING_PATH,
            Router::new()
                .route("/return", get(onboarding_return))
                .route("/refresh", get(onboarding_refresh)),
        )
}

async fn checkout_success() -> impl IntoResponse {
    page(
        "Payment received",
        "You can close this page and return to the app. The app can take a few seconds to update.",
    )
}

async fn checkout_cancel() -> impl IntoResponse {
    page(
        "Checkout cancelled",
        "You can close this page and return to the app.",
    )
}

/// Stripe returns here whether or not onboarding is complete, so the page
/// claims neither; `account.updated` decides.
async fn onboarding_return() -> impl IntoResponse {
    page(
        "Payout setup saved",
        "You can close this page and return to the app. If Stripe still needs details, you can finish setup from the app.",
    )
}

/// Account links are single-use. Stripe's suggestion is to mint a new one here
/// and redirect, but this browser carries no MatrixMedia session to mint it
/// for — so the creator restarts from the app, where `POST /creator/onboard`
/// hands out a fresh link for the existing account.
async fn onboarding_refresh() -> impl IntoResponse {
    page(
        "Setup link expired",
        "This link has expired or was already used. Return to the app and open payout setup again to continue.",
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
        let mut pages = vec![
            (onboarding_return_url(PUBLIC_URL), "Payout setup saved"),
            (onboarding_refresh_url(PUBLIC_URL), "Setup link expired"),
        ];
        for kind in [CheckoutKind::Donation, CheckoutKind::Subscription] {
            pages.push((
                checkout_success_url(PUBLIC_URL, kind, id),
                "Payment received",
            ));
            pages.push((
                checkout_cancel_url(PUBLIC_URL, kind, id),
                "Checkout cancelled",
            ));
        }

        for (url, heading) in pages {
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
            assert!(
                body.to_lowercase().contains("return to the app"),
                "{url}: {body}"
            );
        }
    }

    #[test]
    fn a_trailing_slash_on_public_url_is_not_doubled() {
        let id = Uuid::nil();
        assert_eq!(
            checkout_success_url(
                "https://matrix.example.org/",
                CheckoutKind::Subscription,
                id
            ),
            format!("https://matrix.example.org/_mm/client/v1/checkout/subscriptions/{id}/success"),
        );
        assert_eq!(
            onboarding_return_url("https://matrix.example.org/"),
            "https://matrix.example.org/_mm/client/v1/onboarding/return",
        );
    }

    #[tokio::test]
    async fn only_the_known_outcomes_are_pages() {
        let id = Uuid::new_v4();
        for path in [
            format!("{CHECKOUT_PATH}/subscriptions/{id}/refund"),
            format!("{CHECKOUT_PATH}/tiers/{id}/success"),
            format!("{ONBOARDING_PATH}/complete"),
        ] {
            let (status, _, _) = get_page(&path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn the_pages_are_read_only() {
        for path in [
            checkout_success_url("", CheckoutKind::Subscription, Uuid::new_v4()),
            onboarding_return_url(""),
        ] {
            let resp = router()
                .oneshot(Request::post(&path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED, "{path}");
        }
    }
}
