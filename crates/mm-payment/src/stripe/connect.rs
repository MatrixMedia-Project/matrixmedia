//! Stripe Connect -- creator onboarding via Express accounts.

use crate::provider::{OnboardingRequest, OnboardingResponse, PaymentError};

/// Create a Stripe Connect Express account and return an onboarding URL.
pub async fn create_connected_account(
    client: &stripe::Client,
    req: OnboardingRequest,
) -> Result<OnboardingResponse, PaymentError> {
    // 1. Create Express connected account
    let mut params = stripe::CreateAccount::new();
    params.type_ = Some(stripe::AccountType::Express);
    params.metadata = Some(std::collections::HashMap::from([(
        "mm_user_id".to_string(),
        req.user_id.clone(),
    )]));

    let account = stripe::Account::create(client, params)
        .await
        .map_err(|e| PaymentError::Internal(format!("Stripe account creation failed: {e}")))?;

    let account_id_str = account.id.as_str().to_string();

    // 2. Create account link for onboarding
    let account_link_params = stripe::CreateAccountLink {
        account: account.id,
        type_: stripe::AccountLinkType::AccountOnboarding,
        return_url: Some(&req.return_url),
        refresh_url: Some(&req.refresh_url),
        collect: None,
        collection_options: None,
        expand: &[],
    };

    let link = stripe::AccountLink::create(client, account_link_params)
        .await
        .map_err(|e| PaymentError::Internal(format!("Stripe account link creation failed: {e}")))?;

    // 3. Return onboarding response
    Ok(OnboardingResponse {
        account_id: account_id_str,
        onboarding_url: link.url,
    })
}

/// Check if a Stripe Connect account has completed onboarding.
pub async fn check_onboarding_status(
    client: &stripe::Client,
    account_id: &str,
) -> Result<bool, PaymentError> {
    let id: stripe::AccountId = account_id
        .parse()
        .map_err(|_| PaymentError::InvalidRequest(format!("Invalid account ID: {account_id}")))?;

    let account = stripe::Account::retrieve(client, &id, &[])
        .await
        .map_err(|e| PaymentError::Internal(format!("Stripe account retrieval failed: {e}")))?;

    Ok(account.charges_enabled.unwrap_or(false))
}
