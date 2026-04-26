//! Stripe webhook verification and event parsing.

use crate::provider::{CheckoutMode, PaymentError, WebhookEvent};

/// Verify Stripe webhook signature and parse the event.
///
/// Uses `stripe::Webhook::construct_event()` for HMAC verification.
/// Returns a normalized `WebhookEvent` enum.
pub fn verify_and_parse(
    webhook_secret: &str,
    payload: &[u8],
    signature: &str,
) -> Result<WebhookEvent, PaymentError> {
    let payload_str = std::str::from_utf8(payload)
        .map_err(|e| PaymentError::WebhookInvalid(format!("Invalid UTF-8 payload: {e}")))?;

    // 1. Verify HMAC signature and parse event
    let event = stripe::Webhook::construct_event(payload_str, signature, webhook_secret)
        .map_err(|e| PaymentError::WebhookInvalid(format!("Signature verification failed: {e}")))?;

    // 2. Route by event type
    match event.type_ {
        stripe::EventType::CheckoutSessionCompleted => parse_checkout_completed(&event),
        stripe::EventType::AccountUpdated => parse_account_updated(&event),
        stripe::EventType::CustomerSubscriptionCreated => parse_subscription_created(&event),
        stripe::EventType::CustomerSubscriptionUpdated => parse_subscription_updated(&event),
        stripe::EventType::CustomerSubscriptionDeleted => parse_subscription_cancelled(&event),
        stripe::EventType::InvoicePaymentFailed => parse_payment_failed(&event),
        other => Ok(WebhookEvent::Unknown {
            event_type: other.to_string(),
        }),
    }
}

/// Parse a checkout.session.completed event.
fn parse_checkout_completed(event: &stripe::Event) -> Result<WebhookEvent, PaymentError> {
    match &event.data.object {
        stripe::EventObject::CheckoutSession(session) => {
            let mode = match session.mode {
                stripe::CheckoutSessionMode::Payment => CheckoutMode::Payment,
                stripe::CheckoutSessionMode::Subscription => CheckoutMode::Subscription,
                _ => CheckoutMode::Payment,
            };

            let metadata = session.metadata.clone().unwrap_or_default();

            Ok(WebhookEvent::CheckoutCompleted {
                session_id: session.id.as_str().to_string(),
                mode,
                metadata,
            })
        }
        _ => Err(PaymentError::WebhookInvalid(
            "Expected CheckoutSession object in checkout.session.completed event".to_string(),
        )),
    }
}

/// Parse an account.updated event.
fn parse_account_updated(event: &stripe::Event) -> Result<WebhookEvent, PaymentError> {
    match &event.data.object {
        stripe::EventObject::Account(account) => Ok(WebhookEvent::AccountOnboarded {
            account_id: account.id.as_str().to_string(),
            charges_enabled: account.charges_enabled.unwrap_or(false),
        }),
        _ => Err(PaymentError::WebhookInvalid(
            "Expected Account object in account.updated event".to_string(),
        )),
    }
}

/// Parse a customer.subscription.created event.
fn parse_subscription_created(event: &stripe::Event) -> Result<WebhookEvent, PaymentError> {
    match &event.data.object {
        stripe::EventObject::Subscription(sub) => {
            let metadata = sub.metadata.clone();

            let customer_id = sub.customer.id().as_str().to_string();

            Ok(WebhookEvent::SubscriptionCreated {
                subscription_id: sub.id.as_str().to_string(),
                customer_id,
                metadata,
            })
        }
        _ => Err(PaymentError::WebhookInvalid(
            "Expected Subscription object in customer.subscription.created event".to_string(),
        )),
    }
}

/// Parse a customer.subscription.updated event.
fn parse_subscription_updated(event: &stripe::Event) -> Result<WebhookEvent, PaymentError> {
    match &event.data.object {
        stripe::EventObject::Subscription(sub) => Ok(WebhookEvent::SubscriptionUpdated {
            subscription_id: sub.id.as_str().to_string(),
            status: sub.status.to_string(),
        }),
        _ => Err(PaymentError::WebhookInvalid(
            "Expected Subscription object in customer.subscription.updated event".to_string(),
        )),
    }
}

/// Parse a customer.subscription.deleted event.
fn parse_subscription_cancelled(event: &stripe::Event) -> Result<WebhookEvent, PaymentError> {
    match &event.data.object {
        stripe::EventObject::Subscription(sub) => Ok(WebhookEvent::SubscriptionCancelled {
            subscription_id: sub.id.as_str().to_string(),
        }),
        _ => Err(PaymentError::WebhookInvalid(
            "Expected Subscription object in customer.subscription.deleted event".to_string(),
        )),
    }
}

/// Parse an invoice.payment_failed event.
fn parse_payment_failed(event: &stripe::Event) -> Result<WebhookEvent, PaymentError> {
    match &event.data.object {
        stripe::EventObject::Invoice(invoice) => {
            let subscription_id = invoice
                .subscription
                .as_ref()
                .map(|s| s.id().as_str().to_string());

            // Extract last error message if available from the charge
            let reason = "Payment failed".to_string();

            Ok(WebhookEvent::PaymentFailed {
                subscription_id,
                reason,
            })
        }
        _ => Err(PaymentError::WebhookInvalid(
            "Expected Invoice object in invoice.payment_failed event".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Non-UTF-8 payload must be rejected with WebhookInvalid before the
    /// HMAC step. Defends against panics on binary-garbage input.
    #[test]
    fn rejects_non_utf8_payload() {
        // 0xFF is invalid UTF-8 as a leading byte
        let bad_payload: &[u8] = &[0xFF, 0xFE, 0xFD];
        let err = verify_and_parse("whsec_test", bad_payload, "t=0,v1=deadbeef")
            .expect_err("must reject non-UTF-8 payload");
        match err {
            PaymentError::WebhookInvalid(msg) => {
                assert!(
                    msg.contains("UTF-8") || msg.contains("Invalid"),
                    "Error msg should mention encoding: {msg}"
                );
            }
            other => panic!("Expected WebhookInvalid, got {other:?}"),
        }
    }

    /// A well-formed UTF-8 payload that isn't a valid Stripe event /
    /// doesn't carry a valid HMAC must be rejected at the signature step.
    #[test]
    fn rejects_invalid_signature() {
        let payload = br#"{"id":"evt_test","type":"checkout.session.completed"}"#;
        let err = verify_and_parse("whsec_test_secret_long_enough", payload, "t=0,v1=garbage")
            .expect_err("must reject invalid signature");
        match err {
            PaymentError::WebhookInvalid(msg) => {
                // Stripe SDK may phrase the failure several ways; just
                // ensure we routed it to WebhookInvalid (not Internal).
                assert!(!msg.is_empty(), "Error message should not be empty");
            }
            other => panic!("Expected WebhookInvalid, got {other:?}"),
        }
    }

    /// Empty signature header must reject without panicking.
    #[test]
    fn rejects_empty_signature() {
        let payload = br#"{"id":"evt_test"}"#;
        let err = verify_and_parse("whsec_test", payload, "")
            .expect_err("must reject empty signature");
        assert!(matches!(err, PaymentError::WebhookInvalid(_)));
    }

    /// Empty payload also rejects gracefully.
    #[test]
    fn rejects_empty_payload() {
        let err = verify_and_parse("whsec_test", b"", "t=0,v1=garbage")
            .expect_err("must reject empty payload");
        assert!(matches!(err, PaymentError::WebhookInvalid(_)));
    }

    /// Empty webhook secret should still reject (not silently pass).
    #[test]
    fn rejects_empty_webhook_secret() {
        let payload = br#"{"id":"evt_test"}"#;
        let err = verify_and_parse("", payload, "t=0,v1=anything")
            .expect_err("must reject empty webhook secret");
        assert!(matches!(err, PaymentError::WebhookInvalid(_)));
    }
}
