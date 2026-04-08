//! Stripe webhook verification and event parsing.

use crate::provider::{CheckoutMode, PaymentError, WebhookEvent};
use std::collections::HashMap;

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

/// Extract metadata from a Stripe object's JSON value.
fn _extract_metadata(obj: &serde_json::Value) -> HashMap<String, String> {
    obj.get("metadata")
        .and_then(|m| m.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}
