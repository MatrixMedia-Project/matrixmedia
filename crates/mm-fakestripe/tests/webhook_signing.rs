//! Prove that the webhook signing algorithm mm-fakestripe uses is the exact
//! format async-stripe's `Webhook::construct_event` expects. This is the single
//! most fragile part of the fake — if this test passes, real mm-core will
//! accept webhooks from the fake.
//!
//! We reproduce the signing inline (it's a handful of lines) rather than
//! spawning the server, so we can focus on the crypto.

use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[test]
fn async_stripe_accepts_our_signature_for_checkout_session_completed() {
    let secret = "whsec_test_very_secret_abcdefghijklmnop";

    let event = json!({
        "id": "evt_fake_roundtrip",
        "object": "event",
        "api_version": "2024-11-20.acacia",
        "created": chrono::Utc::now().timestamp(),
        "data": {
            "object": {
                "id": "cs_fake_roundtrip",
                "object": "checkout.session",
                "mode": "payment",
                "url": "https://fake/checkout/cs_fake_roundtrip",
                "status": "complete",
                "payment_status": "paid",
                "payment_intent": "pi_fake_xxx",
                "created": chrono::Utc::now().timestamp(),
                "expires_at": chrono::Utc::now().timestamp() + 1800,
                "livemode": false,
                "payment_method_types": ["card"],
                "currency": "usd",
                "metadata": { "donation_id": "d_test" },
                "success_url": "http://ok",
                "cancel_url": "http://no",
                "automatic_tax": { "enabled": false, "liability": null, "status": null },
                "custom_fields": [],
                "custom_text": {
                    "after_submit": null,
                    "shipping_address": null,
                    "submit": null,
                    "terms_of_service_acceptance": null
                },
                "shipping_options": []
            }
        },
        "livemode": false,
        "pending_webhooks": 0,
        "request": { "id": null, "idempotency_key": null },
        "type": "checkout.session.completed",
    });

    let payload = serde_json::to_string(&event).unwrap();
    let ts = chrono::Utc::now().timestamp();
    let signed_payload = format!("{ts}.{payload}");

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(signed_payload.as_bytes());
    let sig_hex = hex::encode(mac.finalize().into_bytes());
    let header = format!("t={ts},v1={sig_hex}");

    // Now verify with async-stripe's own construct_event.
    let parsed = stripe::Webhook::construct_event(&payload, &header, secret).unwrap_or_else(|e| {
        panic!("async-stripe rejected our signature: {e}\npayload: {payload}")
    });

    assert_eq!(parsed.type_, stripe::EventType::CheckoutSessionCompleted);
    match &parsed.data.object {
        stripe::EventObject::CheckoutSession(session) => {
            assert_eq!(session.id.as_str(), "cs_fake_roundtrip");
            assert_eq!(session.mode, stripe::CheckoutSessionMode::Payment);
            let metadata = session.metadata.as_ref().expect("metadata");
            assert_eq!(metadata.get("donation_id").map(String::as_str), Some("d_test"));
        }
        other => panic!("unexpected event object: {other:?}"),
    }
}

#[test]
fn async_stripe_accepts_our_signature_for_customer_subscription_deleted() {
    let secret = "whsec_test_very_secret_abcdefghijklmnop";
    let now = chrono::Utc::now().timestamp();

    let event = json!({
        "id": "evt_fake_sub_cancel",
        "object": "event",
        "api_version": "2024-11-20.acacia",
        "created": now,
        "data": {
            "object": {
                "id": "sub_fake_roundtrip",
                "object": "subscription",
                "automatic_tax": { "enabled": false, "liability": null, "status": null },
                "billing_cycle_anchor": now - 3600,
                "cancel_at_period_end": false,
                "canceled_at": now,
                "created": now - 3600,
                "currency": "usd",
                "current_period_start": now - 3600,
                "current_period_end": now + 86400,
                "customer": "cus_fake_xxx",
                "items": {
                    "object": "list",
                    "data": [],
                    "has_more": false,
                    "url": "/v1/subscription_items"
                },
                "livemode": false,
                "metadata": {},
                "start_date": now - 3600,
                "status": "canceled"
            }
        },
        "livemode": false,
        "pending_webhooks": 0,
        "request": { "id": null, "idempotency_key": null },
        "type": "customer.subscription.deleted",
    });

    let payload = serde_json::to_string(&event).unwrap();
    let ts = chrono::Utc::now().timestamp();
    let signed_payload = format!("{ts}.{payload}");

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(signed_payload.as_bytes());
    let sig_hex = hex::encode(mac.finalize().into_bytes());
    let header = format!("t={ts},v1={sig_hex}");

    let parsed = stripe::Webhook::construct_event(&payload, &header, secret).unwrap_or_else(|e| {
        panic!("async-stripe rejected our subscription cancel signature: {e}\npayload: {payload}")
    });
    assert_eq!(parsed.type_, stripe::EventType::CustomerSubscriptionDeleted);
    match &parsed.data.object {
        stripe::EventObject::Subscription(sub) => {
            assert_eq!(sub.id.as_str(), "sub_fake_roundtrip");
        }
        other => panic!("unexpected event object: {other:?}"),
    }
}

#[test]
fn async_stripe_accepts_our_signature_for_account_updated() {
    let secret = "whsec_test_very_secret_abcdefghijklmnop";

    let event = json!({
        "id": "evt_fake_acct",
        "object": "event",
        "api_version": "2024-11-20.acacia",
        "created": chrono::Utc::now().timestamp(),
        "data": {
            "object": {
                "id": "acct_fake_roundtrip",
                "object": "account",
                "type": "express",
                "charges_enabled": true,
                "payouts_enabled": true,
                "details_submitted": true,
                "country": "US",
                "default_currency": "usd",
                "deleted": false,
                "email": null,
                "metadata": {}
            }
        },
        "livemode": false,
        "pending_webhooks": 0,
        "request": { "id": null, "idempotency_key": null },
        "type": "account.updated",
    });

    let payload = serde_json::to_string(&event).unwrap();
    let ts = chrono::Utc::now().timestamp();
    let signed_payload = format!("{ts}.{payload}");

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(signed_payload.as_bytes());
    let sig_hex = hex::encode(mac.finalize().into_bytes());
    let header = format!("t={ts},v1={sig_hex}");

    let parsed = stripe::Webhook::construct_event(&payload, &header, secret)
        .expect("async-stripe must accept our account.updated signature");
    assert_eq!(parsed.type_, stripe::EventType::AccountUpdated);
    match &parsed.data.object {
        stripe::EventObject::Account(account) => {
            assert_eq!(account.id.as_str(), "acct_fake_roundtrip");
            assert_eq!(account.charges_enabled, Some(true));
        }
        other => panic!("unexpected event object: {other:?}"),
    }
}
