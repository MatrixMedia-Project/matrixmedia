//! Stripe Checkout Sessions -- for donation payments and subscriptions.

use crate::provider::{CheckoutMode, CheckoutRequest, CheckoutResponse, PaymentError};

/// Create a Stripe Checkout Session.
///
/// For donations: mode = "payment" with transfer_data to creator's connected account.
/// For subscriptions: mode = "subscription", also a destination charge: transfer_data
/// to the creator's connected account, minus application_fee_percent.
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

            params.subscription_data = Some(subscription_data(&req));
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

/// `subscription_data` for a subscription Checkout Session.
///
/// The tier's Price lives on the platform account, so this is a destination
/// charge: each invoice's funds transfer to the creator's connected account
/// and the platform keeps `application_fee_percent`. Stripe accepts that fee
/// only alongside `transfer_data[destination]` (or a `Stripe-Account`
/// header), and with at most two decimal places.
fn subscription_data(req: &CheckoutRequest) -> stripe::CreateCheckoutSessionSubscriptionData {
    let fee_pct = req.platform_fee_cents.map(|fee| {
        // Convert cents-based fee to a percentage of the amount
        // For subscriptions, Stripe uses a percentage (e.g. 10.0 for 10%)
        let pct = match req.amount_cents {
            Some(amount) if amount > 0 => (fee as f64 / amount as f64) * 100.0,
            _ => 0.0,
        };
        (pct * 100.0).round() / 100.0
    });

    stripe::CreateCheckoutSessionSubscriptionData {
        application_fee_percent: fee_pct,
        billing_cycle_anchor: None,
        default_tax_rates: None,
        description: None,
        invoice_settings: None,
        metadata: Some(req.metadata.clone()),
        on_behalf_of: None,
        proration_behavior: None,
        transfer_data: Some(stripe::CreateCheckoutSessionSubscriptionDataTransferData {
            destination: req.creator_account_id.clone(),
            amount_percent: None,
        }),
        trial_end: None,
        trial_period_days: None,
        trial_settings: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A $4.99/month tier: `calculate_fees(499, 0.10)` leaves a 46-cent
    /// platform fee.
    fn subscription_request() -> CheckoutRequest {
        CheckoutRequest {
            mode: CheckoutMode::Subscription,
            amount_cents: Some(499),
            currency: "usd".to_string(),
            creator_account_id: "acct_1CreatorTest".to_string(),
            platform_fee_cents: Some(46),
            success_url: "https://mm.example/subscriptions/s/success".to_string(),
            cancel_url: "https://mm.example/subscriptions/s/cancel".to_string(),
            metadata: HashMap::new(),
            price_id: Some("price_1TierTest".to_string()),
        }
    }

    /// The tier's Price lives on the platform account, so the subscription
    /// is a destination charge: Stripe only accepts application_fee_percent
    /// with transfer_data[destination] (or a Stripe-Account header), and
    /// the creator gets paid only through that transfer.
    #[test]
    fn subscription_pays_the_creator_through_a_destination_transfer() {
        let data = subscription_data(&subscription_request());
        let transfer = data.transfer_data.expect("transfer_data must be set");
        assert_eq!(transfer.destination, "acct_1CreatorTest");
        assert_eq!(transfer.amount_percent, None);
    }

    /// Stripe rejects an application_fee_percent with more than two
    /// decimal places; 46 / 499 is 9.2184…%.
    #[test]
    fn subscription_fee_percent_has_at_most_two_decimals() {
        let data = subscription_data(&subscription_request());
        assert_eq!(data.application_fee_percent, Some(9.22));
    }
}
