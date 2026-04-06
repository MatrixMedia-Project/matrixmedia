use axum::{Json, Router, extract::State, routing::put};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use crate::error::ApiError;
use crate::middleware::AppserviceAuth;
use crate::state::SharedState;

/// Build appservice transaction routes.
///
/// The homeserver pushes events to `PUT /_mm/appservice/transactions/:txn_id`.
pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/transactions/{txn_id}", put(handle_transaction))
        .with_state(state)
}

/// Request body for an appservice transaction.
#[derive(Debug, Deserialize)]
struct TransactionBody {
    #[serde(default)]
    events: Vec<serde_json::Value>,
}

/// PUT /transactions/:txn_id -- Handle an incoming appservice transaction.
///
/// The homeserver sends batches of events. We acknowledge immediately
/// and process events via the `AppserviceHandler`.
async fn handle_transaction(
    _auth: AppserviceAuth,
    State(state): State<SharedState>,
    axum::extract::Path(txn_id): axum::extract::Path<String>,
    Json(body): Json<TransactionBody>,
) -> Result<Json<Value>, ApiError> {
    debug!(
        txn_id = %txn_id,
        event_count = body.events.len(),
        "Processing appservice transaction"
    );

    // Process events through the appservice handler.
    // Errors on individual events are logged internally; the transaction
    // itself always succeeds (appservice spec requirement).
    let _ = state
        .appservice_handler
        .handle_transaction(body.events)
        .await;

    // Appservice spec requires an empty JSON object response.
    Ok(Json(serde_json::json!({})))
}
