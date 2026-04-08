//! Stripe Checkout Sessions -- for donation payments and subscriptions.

use crate::provider::{CheckoutMode, CheckoutRequest, CheckoutResponse, PaymentError};

/// Create a Stripe Checkout Session.
///
/// For donations: mode = "payment" with transfer_data to creator's connected account.
/// For subscriptions: mode = "subscription" with application_fee_percent.
pub async fn create_checkout_session(
    client: &stripe::Client,
    req: CheckoutRequest,
) -> Result<CheckoutResponse, PaymentError> {
    let currency: stripe::Currency = req.currency.parse().map_err(|_| {
        PaymentError::InvalidRequest(format!("Unsupported currency: {}", req.currency))
    })?;

    let mut params = stripe::CreateCheckoutSession::new();

    // Set common fields
    params.success_url = Some(&req.success_url);
    params.cancel_url = Some(&req.cancel_url);
    params.metadata = Some(req.metadata.clone());

    match req.mode {
        CheckoutMode::Payment => {
            params.mode = Some(stripe::CheckoutSessionMode::Payment);

            // Line item with inline price_data for one-time payment
            let amount_cents = req.amount_cents.ok_or_else(|| {
                PaymentError::InvalidRequest("amount_cents required for payment mode".to_string())
            })?;

            let line_item = stripe::CreateCheckoutSessionLineItems {
                adjustable_quantity: None,
                dynamic_tax_rates: None,
                price: None,
                price_data: Some(stripe::CreateCheckoutSessionLineItemsPriceData {
                    currency,
                    product: None,
                    product_data: Some(
                        stripe::CreateCheckoutSessionLineItemsPriceDataProductData {
                            name: "Donation".to_string(),
                            description: Some("MatrixMedia stream donation".to_string()),
                            images: None,
                            metadata: None,
                            tax_code: None,
                        },
                    ),
                    recurring: None,
                    tax_behavior: None,
                    unit_amount: Some(amount_cents),
                    unit_amount_decimal: None,
                }),
                quantity: Some(1),
                tax_rates: None,
            };
            params.line_items = Some(vec![line_item]);

            // Set transfer_data and application_fee for Connect payouts
            params.payment_intent_data = Some(stripe::CreateCheckoutSessionPaymentIntentData {
                application_fee_amount: req.platform_fee_cents,
                transfer_data: Some(stripe::CreateCheckoutSessionPaymentIntentDataTransferData {
                    destination: req.creator_account_id.clone(),
                    amount: None,
                }),
                capture_method: None,
                description: None,
                metadata: None,
                on_behalf_of: None,
                receipt_email: None,
                setup_future_usage: None,
                shipping: None,
                statement_descriptor: None,
                statement_descriptor_suffix: None,
                transfer_group: None,
            });
        }
        CheckoutMode::Subscription => {
            params.mode = Some(stripe::CheckoutSessionMode::Subscription);

            // For subscription mode, a price_id is required
            let price_id = req.price_id.as_deref().ok_or_else(|| {
                PaymentError::InvalidRequest("price_id required for subscription mode".to_string())
            })?;

            let line_item = stripe::CreateCheckoutSessionLineItems {
                adjustable_quantity: None,
                dynamic_tax_rates: None,
                price: Some(price_id.to_string()),
                price_data: None,
                quantity: Some(1),
                tax_rates: None,
            };
            params.line_items = Some(vec![line_item]);

            // Set application_fee_percent on subscription_data
            let fee_pct = req.platform_fee_cents.map(|fee| {
                // Convert cents-based fee to a percentage of the amount
                // For subscriptions, Stripe uses a percentage (e.g. 10.0 for 10%)
                if let Some(amount) = req.amount_cents {
                    if amount > 0 {
                        (fee as f64 / amount as f64) * 100.0
                    } else {
                        0.0
                    }
                } else {
                    0.0
                }
            });

            params.subscription_data = Some(stripe::CreateCheckoutSessionSubscriptionData {
                application_fee_percent: fee_pct,
                billing_cycle_anchor: None,
                default_tax_rates: None,
                description: None,
                invoice_settings: None,
                metadata: Some(req.metadata.clone()),
                on_behalf_of: None,
                proration_behavior: None,
                transfer_data: None,
                trial_end: None,
                trial_period_days: None,
                trial_settings: None,
            });
        }
    }

    // Create the session
    let session = stripe::CheckoutSession::create(client, params)
        .await
        .map_err(|e| {
            PaymentError::PaymentFailed(format!("Stripe checkout creation failed: {e}"))
        })?;

    let checkout_url = session
        .url
        .ok_or_else(|| PaymentError::Internal("Stripe returned no checkout URL".to_string()))?;

    Ok(CheckoutResponse {
        session_id: session.id.as_str().to_string(),
        checkout_url,
    })
}
