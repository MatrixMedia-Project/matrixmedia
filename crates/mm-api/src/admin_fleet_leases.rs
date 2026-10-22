//! Admin API for manual relay leases (Task 10, spec §5.1/§5.2, §8.2 R5): list the fleet's
//! leased fan-out relays, rent new ones outside the autoscaler, and release one early.
//!
//! All three handlers sit behind [`AdminAuth`] like `admin_fleet_providers`; the demo role sees
//! the same shape, minus anything an operator would not want a demo viewer reading (node ids,
//! `created_by`, cost figures) — mirrored from how `gpu_nodes` shapes its own view for demo.
//!
//! The database work is [`lease_db::{create, release}`]; this module's job is the HTTP shape
//! around them (typed confirmation, idempotent replay, force semantics) plus the list view,
//! which this module builds itself (cost-to-date from `relay_hour` usage rows written by
//! `lease_meter`, prices from `mm_fleet_provider_status.prices`).

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use mm_fleet::lease_db::{self, LeaseCreate, LeaseError, ReleaseState};
use mm_fleet::providers_db as pdb;
use mm_core::fleet::NodeId;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::ApiError;
use crate::middleware::AdminAuth;

/// The confirmation string an operator must type to rent a relay outside the autoscaler — same
/// pattern as `admin_fleet_providers::TEST_BOOT_CONFIRMATION`.
const RENT_RELAY_CONFIRMATION: &str = "rent relay";

pub fn routes(pool: PgPool) -> Router {
    Router::new()
        .route(
            "/broadcast-servers/leases",
            get(list_leases).post(create_lease),
        )
        .route(
            "/broadcast-servers/leases/{node_id}",
            axum::routing::delete(release_lease),
        )
        .with_state(pool)
}

// ---- errors ---------------------------------------------------------------------------

pub enum LeasesApiError {
    BadRequest(String),
    NotFound,
    Conflict { code: &'static str, message: String },
    Admin(ApiError),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for LeasesApiError {
    fn from(e: sqlx::Error) -> Self {
        LeasesApiError::Db(e)
    }
}

impl From<ApiError> for LeasesApiError {
    fn from(e: ApiError) -> Self {
        LeasesApiError::Admin(e)
    }
}

impl From<LeaseError> for LeasesApiError {
    fn from(e: LeaseError) -> Self {
        match e {
            LeaseError::InvalidCount(_) | LeaseError::InvalidDurationHint { .. } => {
                LeasesApiError::BadRequest(e.to_string())
            }
            LeaseError::CountMismatch { .. } => LeasesApiError::Conflict {
                code: "MM_LEASE_REQUEST_MISMATCH",
                message: e.to_string(),
            },
            LeaseError::NotFound(_) => LeasesApiError::NotFound,
            LeaseError::AlreadyGone { .. } => LeasesApiError::Conflict {
                code: "MM_LEASE_ALREADY_GONE",
                message: e.to_string(),
            },
            LeaseError::Db(err) => LeasesApiError::Db(err),
        }
    }
}

impl IntoResponse for LeasesApiError {
    fn into_response(self) -> Response {
        match self {
            LeasesApiError::BadRequest(m) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "MM_INVALID_REQUEST", "message": m})),
            )
                .into_response(),
            LeasesApiError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "MM_NOT_FOUND", "message": "no such leased node"})),
            )
                .into_response(),
            LeasesApiError::Conflict { code, message } => {
                (StatusCode::CONFLICT, Json(json!({"error": code, "message": message})))
                    .into_response()
            }
            LeasesApiError::Admin(e) => e.into_response(),
            LeasesApiError::Db(e) => {
                tracing::error!(error = %e, "fleet leases db error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "MM_INTERNAL", "message": "database error"})),
                )
                    .into_response()
            }
        }
    }
}

type R<T> = Result<T, LeasesApiError>;

fn bad(message: impl Into<String>) -> LeasesApiError {
    LeasesApiError::BadRequest(message.into())
}

fn body<T>(b: Result<Json<T>, JsonRejection>, shape: &str) -> R<T> {
    b.map(|Json(v)| v)
        .map_err(|_| LeasesApiError::BadRequest(format!("the body must be {shape}")))
}

// ---- list -------------------------------------------------------------------------------

#[derive(Serialize)]
pub struct LeaseView {
    pub node_id: String,
    pub provider_label: Option<String>,
    pub kind: Option<String>,
    pub region: Option<String>,
    pub state: String,
    pub viewers_current: i32,
    pub viewer_capacity: Option<i32>,
    pub started_at: Option<DateTime<Utc>>,
    pub cost_to_date_minor: Option<i64>,
    pub currency: Option<&'static str>,
    pub unfunded_share_pct: Option<f64>,
    pub draining_overdue: bool,
}

#[derive(Default)]
struct LeaseUsage {
    total_milli: i64,
    house_milli: i64,
}

/// `relay_hour` usage rows for a set of nodes, totalled and split by payer — the two numbers
/// [`list_leases`] needs: total milli (for cost-to-date) and the `house` slice (for
/// `unfunded_share_pct`, R4's uncovered-broadcast rule made visible per node).
async fn usage_by_node(pool: &PgPool, node_ids: &[String]) -> sqlx::Result<HashMap<String, LeaseUsage>> {
    if node_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "SELECT mm_node_id, payer, SUM(quantity_milli) AS total
           FROM mm_usage_events
          WHERE unit = 'relay_hour' AND mm_node_id = ANY($1)
          GROUP BY mm_node_id, payer",
    )
    .bind(node_ids)
    .fetch_all(pool)
    .await?;
    let mut out: HashMap<String, LeaseUsage> = HashMap::new();
    for r in rows {
        let node_id: String = r.get("mm_node_id");
        let payer: String = r.get("payer");
        let total: i64 = r.get::<i64, _>("total");
        let entry = out.entry(node_id).or_default();
        entry.total_milli += total;
        if payer == "house" {
            entry.house_milli += total;
        }
    }
    Ok(out)
}

/// `true` when a node is draining and its deadline — the point the sweeper reaps it by — has
/// already passed: the sweep is due but has not yet run, which the operator should be able to
/// see rather than infer from a stale "draining" with no further signal.
fn is_draining_overdue(state: &str, destroy_deadline: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    state == "draining" && destroy_deadline.is_some_and(|d| d <= now)
}

async fn list_leases(auth: AdminAuth, State(pool): State<PgPool>) -> R<Json<Vec<LeaseView>>> {
    let demo = auth.is_demo();
    let now = Utc::now();
    let rows = sqlx::query(
        "SELECT n.mm_node_id, n.state, n.viewers_current, n.viewer_capacity, n.billing_started_at,
                n.destroy_deadline, n.size, n.provider_ref, p.label, p.kind, d.region
           FROM mm_fleet_nodes n
           JOIN mm_fleet_desired d ON d.mm_node_id = n.mm_node_id
           LEFT JOIN mm_fleet_providers p ON p.id = n.provider_ref
          WHERE n.flavor = 'fanout' AND n.ownership = 'rented' AND d.lease_source = 'manual'
            AND n.state <> 'gone'
          ORDER BY n.mm_node_id",
    )
    .fetch_all(&pool)
    .await?;

    let prices: HashMap<String, serde_json::Value> = sqlx::query(
        "SELECT provider_id, prices FROM mm_fleet_provider_status",
    )
    .fetch_all(&pool)
    .await?
    .into_iter()
    .map(|r| (r.get::<String, _>("provider_id"), r.get::<serde_json::Value, _>("prices")))
    .collect();

    let node_ids: Vec<String> = rows.iter().map(|r| r.get("mm_node_id")).collect();
    let usage = usage_by_node(&pool, &node_ids).await?;

    let views = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let node_id: String = r.get("mm_node_id");
            let state: String = r.get("state");
            let viewers_current: i32 = r.get("viewers_current");
            let viewer_capacity: Option<i32> = r.get("viewer_capacity");
            let billing_started_at: Option<DateTime<Utc>> = r.get("billing_started_at");
            let destroy_deadline: Option<DateTime<Utc>> = r.get("destroy_deadline");
            let size: Option<String> = r.get("size");
            let provider_ref: Option<String> = r.get("provider_ref");
            let label: Option<String> = r.get("label");
            let kind: Option<String> = r.get("kind");
            let region: Option<String> = r.get("region");

            let price_per_hour = provider_ref
                .as_ref()
                .and_then(|pid| prices.get(pid))
                .zip(size.as_ref())
                .and_then(|(prices, size)| prices.get(size))
                .and_then(|v| v.as_f64());
            let currency = kind.as_deref().map(pdb::price_currency);
            let (cost_to_date_minor, unfunded_share_pct) = match usage.get(&node_id) {
                Some(u) if u.total_milli > 0 => {
                    let cost_minor = price_per_hour
                        .filter(|p| p.is_finite() && *p >= 0.0)
                        .map(|p| ((u.total_milli as f64 / 1000.0) * p * 100.0).round() as i64);
                    let unfunded = (u.house_milli as f64 * 100.0 / u.total_milli as f64).clamp(0.0, 100.0);
                    (cost_minor, Some(unfunded))
                }
                _ => (None, None),
            };
            let overdue = is_draining_overdue(&state, destroy_deadline, now);

            LeaseView {
                node_id: if demo { format!("lease-{i}") } else { node_id },
                provider_label: if demo { None } else { label },
                kind,
                region,
                state,
                viewers_current,
                viewer_capacity,
                started_at: billing_started_at,
                cost_to_date_minor: if demo { None } else { cost_to_date_minor },
                currency,
                unfunded_share_pct: if demo { None } else { unfunded_share_pct },
                draining_overdue: overdue,
            }
        })
        .collect();
    Ok(Json(views))
}

// ---- create (rent) ------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateLeaseBody {
    request_id: Uuid,
    region: String,
    provider_id: Option<String>,
    count: u8,
    /// How long the rolling window reaches, in seconds. `None` defaults to
    /// `lease_db::DEFAULT_DURATION_HINT`; bounds-checked by `lease_db::create` itself.
    duration_hint_secs: Option<i64>,
    confirmation: Option<String>,
}

#[derive(Serialize)]
struct CreateLeaseResponse {
    node_ids: Vec<String>,
}

async fn create_lease(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    b: Result<Json<CreateLeaseBody>, JsonRejection>,
) -> R<(StatusCode, Json<CreateLeaseResponse>)> {
    auth.require_admin()?;
    let b = body(
        b,
        "{request_id, region, provider_id?, count, duration_hint_secs?, confirmation: \"rent relay\"}",
    )?;
    if b.confirmation.as_deref() != Some(RENT_RELAY_CONFIRMATION) {
        return Err(bad(format!(
            "type \"{RENT_RELAY_CONFIRMATION}\" to confirm: it rents 1 to 5 relays outside the autoscaler"
        )));
    }
    let region = b.region.trim();
    if region.is_empty() {
        return Err(bad("region must not be empty"));
    }
    if !(1..=5).contains(&b.count) {
        return Err(bad("count must be between 1 and 5"));
    }

    // A replay (same request_id) is distinguished from a brand-new create only by whether a
    // row for it already exists — the 200-vs-202 split the brief asks for.
    // known race: existed_before read outside create's tx; both callers may see 202,
    // node_ids identical.
    let existed_before: bool = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM mm_fleet_desired WHERE lease_request_id = $1)",
    )
    .bind(b.request_id)
    .fetch_one(&pool)
    .await?;

    let node_ids = lease_db::create(
        &pool,
        &LeaseCreate {
            request_id: b.request_id,
            region: region.to_string(),
            provider_id: b.provider_id.clone(),
            count: b.count,
            created_by: auth.actor(),
            duration_hint: b.duration_hint_secs.map(Duration::seconds),
        },
    )
    .await?;

    let node_ids: Vec<String> = node_ids
        .into_iter()
        .map(|id| id.as_str().to_string())
        .collect();
    // One audit row per CREATE that wrote (or re-confirmed) the lease — a replay is itself a
    // mutation attempt an operator may want to see, so it is recorded too, with the flag.
    pdb::append_audit(
        &pool,
        &pdb::AuditEntry {
            actor: &auth.actor(),
            action: "lease.create",
            target: &b.request_id.to_string(),
            reason: None,
            detail: json!({
                "region": region,
                "provider_id": b.provider_id,
                "count": b.count,
                "duration_hint_secs": b.duration_hint_secs,
                "node_ids": node_ids,
                "replay": existed_before,
            }),
        },
    )
    .await?;

    let status = if existed_before {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(CreateLeaseResponse { node_ids })))
}

// ---- release (drain / force) -------------------------------------------------------------

#[derive(Deserialize)]
struct ReleaseQuery {
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
struct ReleaseResponse {
    state: &'static str,
}

async fn release_lease(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    Path(node_id): Path<String>,
    Query(q): Query<ReleaseQuery>,
) -> R<(StatusCode, Json<ReleaseResponse>)> {
    auth.require_admin()?;
    let state = lease_db::release(&pool, &NodeId::new(&node_id), q.force).await?;
    let word = match state {
        ReleaseState::Draining => "draining",
        ReleaseState::Evicted => "evicted",
    };
    pdb::append_audit(
        &pool,
        &pdb::AuditEntry {
            actor: &auth.actor(),
            action: "lease.release",
            target: &node_id,
            reason: None,
            detail: json!({ "force": q.force, "state": word }),
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(ReleaseResponse { state: word })))
}
