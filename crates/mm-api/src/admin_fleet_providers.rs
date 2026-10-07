//! GPU provider profiles, priority, write-only credentials, bench results and operator
//! requests (spec §8.3, P-A subset). mm-core stores ciphertext it cannot read and never
//! returns it; the runner (a separate process) does the opening and the checking.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use mm_fleet::control_db;
use mm_fleet::endpoint::ip_is_forbidden;
use mm_fleet::providers_db::{
    self as pdb, AuditEntry, CredentialBlob, CredentialSummary, KINDS, ProviderFull, ProviderInput,
    REGIONS, ROLES, StatusRow, ZoneRow,
};
use mm_fleet::requests_db as rq;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use url::Host;

use crate::error::ApiError;
use crate::middleware::AdminAuth;

pub fn routes(pool: PgPool) -> Router {
    Router::new()
        .route(
            "/broadcast-servers/providers",
            get(list_providers).post(create_provider),
        )
        .route("/broadcast-servers/providers/order", put(set_order))
        .route(
            "/broadcast-servers/providers/{id}",
            put(update_provider).delete(delete_provider),
        )
        .route(
            "/broadcast-servers/providers/{id}/credential",
            put(put_credential).delete(clear_credential),
        )
        .route(
            "/broadcast-servers/providers/{id}/bench",
            post(record_bench),
        )
        .route(
            "/broadcast-servers/providers/{id}/requests",
            post(create_request),
        )
        .route("/broadcast-servers/requests/{id}", get(get_request))
        .with_state(pool)
}

// ---- errors ---------------------------------------------------------------------------

pub enum ProvidersApiError {
    BadRequest(String),
    NotFound,
    Conflict { code: &'static str, message: String },
    Admin(ApiError),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for ProvidersApiError {
    fn from(e: sqlx::Error) -> Self {
        ProvidersApiError::Db(e)
    }
}

impl From<ApiError> for ProvidersApiError {
    fn from(e: ApiError) -> Self {
        ProvidersApiError::Admin(e)
    }
}

impl IntoResponse for ProvidersApiError {
    fn into_response(self) -> Response {
        match self {
            ProvidersApiError::BadRequest(m) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "MM_INVALID_REQUEST", "message": m})),
            )
                .into_response(),
            ProvidersApiError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "MM_NOT_FOUND", "message": "no such provider or request"})),
            )
                .into_response(),
            ProvidersApiError::Conflict { code, message } => (
                StatusCode::CONFLICT,
                Json(json!({"error": code, "message": message})),
            )
                .into_response(),
            ProvidersApiError::Admin(e) => e.into_response(),
            ProvidersApiError::Db(e) => {
                tracing::error!(error = %e, "fleet providers db error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "MM_INTERNAL", "message": "database error"})),
                )
                    .into_response()
            }
        }
    }
}

type R<T> = Result<T, ProvidersApiError>;

fn body<T>(b: Result<Json<T>, JsonRejection>, shape: &str) -> R<T> {
    // serde's message can quote the input; name the shape instead.
    b.map(|Json(v)| v)
        .map_err(|_| ProvidersApiError::BadRequest(format!("the body must be {shape}")))
}

// ---- views ----------------------------------------------------------------------------

#[derive(Serialize)]
pub struct RunnerView {
    pub reporting: bool,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub version: Option<String>,
    pub key_fingerprint: Option<String>,
    pub public_key_hex: Option<String>,
    pub fleet_mode_seen: Option<String>,
    pub rented_nodes: Option<i64>,
}

#[derive(Serialize)]
pub struct ProviderView {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub enabled: bool,
    pub priority: i32,
    pub endpoint_display: String,
    pub account_display: Option<String>,
    pub image: String,
    pub gpu_image: String,
    pub transcode_image: Option<String>,
    pub max_gpu_nodes: i32,
    pub bench_state: String,
    pub bench_note: Option<String>,
    pub billing_clock: &'static str,
    pub prepaid: bool,
    pub terraform_module: Option<&'static str>,
    pub default_endpoint: Option<&'static str>,
    pub zones: Vec<ZoneRow>,
    pub credential: Option<CredentialSummary>,
    pub credential_set: bool,
    pub status: Option<StatusRow>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct ProvidersResponse {
    pub demo: bool,
    pub runner: RunnerView,
    pub providers: Vec<ProviderView>,
}

/// The runner as the page may show it, and whether it is fresh. A row older than the
/// staleness window is "not reporting": its key and status claims are withheld, because a
/// token sealed to a runner that is not there can never be opened.
fn runner_view(row: Option<control_db::ControlRow>, now: DateTime<Utc>) -> (RunnerView, bool) {
    match row {
        Some(r) if !control_db::is_stale(&r, now) => {
            let rented = r.detail.get("rented_nodes").and_then(|v| v.as_i64());
            (
                RunnerView {
                    reporting: true,
                    heartbeat_at: Some(r.heartbeat_at),
                    version: Some(r.runner_version),
                    key_fingerprint: Some(r.key_fingerprint),
                    public_key_hex: Some(hex::encode(&r.public_key)),
                    fleet_mode_seen: Some(r.fleet_mode_seen),
                    rented_nodes: rented,
                },
                true,
            )
        }
        Some(r) => (
            RunnerView {
                reporting: false,
                heartbeat_at: Some(r.heartbeat_at),
                version: Some(r.runner_version),
                key_fingerprint: None,
                public_key_hex: None,
                fleet_mode_seen: None,
                rented_nodes: None,
            },
            false,
        ),
        None => (
            RunnerView {
                reporting: false,
                heartbeat_at: None,
                version: None,
                key_fingerprint: None,
                public_key_hex: None,
                fleet_mode_seen: None,
                rented_nodes: None,
            },
            false,
        ),
    }
}

/// What the demo role may see of a status: the verdict and when it was reached, not the
/// provider-account values (balance, quota, stock, prices, the provider's own error text).
fn demo_status(s: StatusRow) -> StatusRow {
    StatusRow {
        key_scope: None,
        quota: json!({}),
        stock: json!({}),
        prices: json!({}),
        balance_minor: None,
        last_error: None,
        ..s
    }
}

fn provider_view(p: ProviderFull, demo: bool, runner_fresh: bool) -> ProviderView {
    let r = p.row;
    let status = p
        .status
        .filter(|_| runner_fresh)
        .map(|s| if demo { demo_status(s) } else { s });
    ProviderView {
        billing_clock: pdb::billing_clock(&r.kind),
        prepaid: pdb::prepaid(&r.kind),
        terraform_module: pdb::terraform_module(&r.kind),
        default_endpoint: pdb::default_endpoint(&r.kind),
        id: r.id,
        label: r.label,
        kind: r.kind,
        enabled: r.enabled,
        priority: r.priority,
        endpoint_display: r.endpoint_display,
        account_display: if demo { None } else { r.account_display },
        image: r.image,
        gpu_image: r.gpu_image,
        transcode_image: r.transcode_image,
        max_gpu_nodes: r.max_gpu_nodes,
        bench_state: r.bench_state,
        bench_note: r.bench_note,
        credential_set: p.credential.is_some(),
        credential: if demo { None } else { p.credential },
        status,
        zones: p.zones,
        updated_at: r.updated_at,
    }
}

// ---- validation -----------------------------------------------------------------------

/// https, no user-info, a host that is not a private or local address. A literal address is
/// judged by the same rule the runner applies before every use (`ip_is_forbidden`); a name
/// is not resolved here (the runner resolves, and vets what it got, each time it dials), so
/// only the names that are local by definition are refused. Messages name the rule, never
/// the value.
pub fn validate_endpoint(s: &str) -> Result<(), String> {
    let url =
        url::Url::parse(s).map_err(|_| "endpoint must be an absolute https URL".to_string())?;
    if url.scheme() != "https" {
        return Err("endpoint must use https".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("endpoint must not carry credentials".into());
    }
    let private = match url.host() {
        None => return Err("endpoint must have a host".into()),
        Some(Host::Ipv4(ip)) => ip_is_forbidden(ip.into()),
        Some(Host::Ipv6(ip)) => ip_is_forbidden(ip.into()),
        Some(Host::Domain(name)) => {
            // `localhost.` is the same name with the root label spelled out.
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost") || name.ends_with(".local")
        }
    };
    if private {
        return Err("endpoint must not point at a private or local address".into());
    }
    Ok(())
}

pub fn validate_input(i: &ProviderInput) -> Result<(), String> {
    let len = |s: &str| s.chars().count();
    if !(1..=80).contains(&len(&i.label)) {
        return Err("label must be 1 to 80 characters".into());
    }
    if !KINDS.contains(&i.kind.as_str()) {
        return Err("kind must be one of scaleway, runpod, akamai, ovh, gcp".into());
    }
    validate_endpoint(&i.endpoint_display)?;
    if !(1..=120).contains(&len(&i.image)) || !(1..=120).contains(&len(&i.gpu_image)) {
        return Err("image and gpu_image must be 1 to 120 characters".into());
    }
    if i.transcode_image.as_deref().map(len).unwrap_or(0) > 120 {
        return Err("transcode_image must be at most 120 characters".into());
    }
    if let Some(a) = &i.account_display {
        if len(a) > 120 {
            return Err("account must be at most 120 characters".into());
        }
    }
    if !(0..=100).contains(&i.max_gpu_nodes) {
        return Err("max_gpu_nodes must be 0 to 100".into());
    }
    if i.kind == "scaleway" && i.zones.is_empty() {
        return Err("at least one zone is required".into());
    }
    let mut seen = std::collections::HashSet::new();
    for z in &i.zones {
        let ok = (2..=32).contains(&z.zone.len())
            && z.zone
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok {
            return Err("zone names are 2 to 32 lowercase letters, digits or dashes".into());
        }
        if !seen.insert(z.zone.as_str()) {
            return Err("zones must be unique".into());
        }
        if !REGIONS.contains(&z.region.as_str()) {
            return Err("region must be eu, us or asia".into());
        }
        for (role, size) in &z.sizes {
            if !ROLES.contains(&role.as_str()) {
                return Err("size roles must be fanout, edge or transcode".into());
            }
            if !(1..=64).contains(&len(size)) {
                return Err("sizes must be 1 to 64 characters".into());
            }
        }
    }
    Ok(())
}

fn hex_bytes(s: &str, min: usize, max: usize, what: &str) -> Result<Vec<u8>, String> {
    let b = hex::decode(s).map_err(|_| format!("{what} must be hex"))?;
    if b.len() < min || b.len() > max {
        return Err(format!("{what} has the wrong length"));
    }
    Ok(b)
}

// ---- handlers -------------------------------------------------------------------------

async fn list_providers(auth: AdminAuth, State(pool): State<PgPool>) -> R<Json<ProvidersResponse>> {
    let (runner, fresh) = runner_view(control_db::read(&pool).await?, Utc::now());
    let demo = auth.is_demo();
    let providers = pdb::list(&pool)
        .await?
        .into_iter()
        .map(|p| provider_view(p, demo, fresh))
        .collect();
    Ok(Json(ProvidersResponse {
        demo,
        runner,
        providers,
    }))
}

const INPUT_SHAPE: &str = "{label, kind, enabled, endpoint_display, account_display?, image, gpu_image, transcode_image?, max_gpu_nodes, zones: [{zone, region, sizes: {role: size}}]}";

async fn create_provider(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    b: Result<Json<ProviderInput>, JsonRejection>,
) -> R<(StatusCode, Json<serde_json::Value>)> {
    auth.require_admin()?;
    let input = body(b, INPUT_SHAPE)?;
    validate_input(&input).map_err(ProvidersApiError::BadRequest)?;
    let id = pdb::insert(&pool, &input).await?;
    pdb::append_audit(
        &pool,
        &AuditEntry {
            actor: &auth.actor(),
            action: "provider_create",
            target: &id,
            reason: None,
            detail: json!({"label": input.label, "kind": input.kind}),
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

async fn update_provider(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    Path(id): Path<String>,
    b: Result<Json<ProviderInput>, JsonRejection>,
) -> R<StatusCode> {
    auth.require_admin()?;
    let input = body(b, INPUT_SHAPE)?;
    validate_input(&input).map_err(ProvidersApiError::BadRequest)?;
    let Some(existing) = pdb::get(&pool, &id).await? else {
        return Err(ProvidersApiError::NotFound);
    };
    if existing.row.kind != input.kind {
        return Err(ProvidersApiError::BadRequest(
            "kind cannot change; create a new provider".into(),
        ));
    }
    if !pdb::update(&pool, &id, &input).await? {
        return Err(ProvidersApiError::NotFound);
    }
    pdb::append_audit(
        &pool,
        &AuditEntry {
            actor: &auth.actor(),
            action: "provider_update",
            target: &id,
            reason: None,
            detail: json!({
                "label": input.label,
                "endpoint_changed": existing.row.endpoint_display != input.endpoint_display,
            }),
        },
    )
    .await?;
    Ok(StatusCode::OK)
}

async fn delete_provider(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    Path(id): Path<String>,
) -> R<StatusCode> {
    auth.require_admin()?;
    match pdb::soft_delete(&pool, &id).await {
        Ok(true) => {
            pdb::append_audit(
                &pool,
                &AuditEntry {
                    actor: &auth.actor(),
                    action: "provider_delete",
                    target: &id,
                    reason: None,
                    detail: json!({}),
                },
            )
            .await?;
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err(ProvidersApiError::NotFound),
        Err(pdb::DeleteRefused::NodesExist(n)) => Err(ProvidersApiError::Conflict {
            code: "MM_FLEET_PROVIDER_IN_USE",
            message: format!("{n} server(s) still reference this provider; release them first"),
        }),
        Err(pdb::DeleteRefused::Db(e)) => Err(e.into()),
    }
}

#[derive(Deserialize)]
struct OrderBody {
    ids: Vec<String>,
}

async fn set_order(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    b: Result<Json<OrderBody>, JsonRejection>,
) -> R<StatusCode> {
    auth.require_admin()?;
    let OrderBody { ids } = body(b, "{ids: [provider id, ...]}")?;
    let before: Vec<String> = pdb::list(&pool)
        .await?
        .into_iter()
        .map(|p| p.row.id)
        .collect();
    match pdb::set_order(&pool, &ids).await {
        Ok(()) => {}
        Err(pdb::OrderError::NotTheSameSet) => {
            return Err(ProvidersApiError::BadRequest(
                "the order must list every provider exactly once".into(),
            ));
        }
        Err(pdb::OrderError::Db(e)) => return Err(e.into()),
    }
    pdb::append_audit(
        &pool,
        &AuditEntry {
            actor: &auth.actor(),
            action: "provider_order",
            target: "providers",
            reason: None,
            detail: json!({"before": before, "after": ids}),
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CredentialBody {
    key_id: String,
    enc: String,
    ciphertext: String,
}

async fn put_credential(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    Path(id): Path<String>,
    b: Result<Json<CredentialBody>, JsonRejection>,
) -> R<StatusCode> {
    auth.require_admin()?;
    let c = body(b, "{key_id, enc: hex, ciphertext: hex}")?;
    if pdb::get(&pool, &id).await?.is_none() {
        return Err(ProvidersApiError::NotFound);
    }
    // The browser sealed to the key the page was showing. It is only good if that is still
    // the key of a runner that is there to open it.
    let runner = control_db::read(&pool).await?;
    let key_changed = || {
        ProvidersApiError::Conflict {
        code: "MM_FLEET_RUNNER_KEY_CHANGED",
        message: "the runner's key is not the one this token was sealed to; reload and enter the token again".into(),
    }
    };
    let Some(runner) = runner else {
        return Err(key_changed());
    };
    if runner.key_fingerprint != c.key_id {
        return Err(key_changed());
    }
    if control_db::is_stale(&runner, Utc::now()) {
        return Err(ProvidersApiError::Conflict {
            code: "MM_FLEET_RUNNER_NOT_REPORTING",
            message:
                "the runner is not reporting; wait for its heartbeat and enter the token again"
                    .into(),
        });
    }
    let enc = hex_bytes(&c.enc, 32, 32, "enc").map_err(ProvidersApiError::BadRequest)?;
    let ct = hex_bytes(&c.ciphertext, 16, 65_536, "ciphertext")
        .map_err(ProvidersApiError::BadRequest)?;
    pdb::put_credential(
        &pool,
        &id,
        &CredentialBlob {
            key_id: c.key_id.clone(),
            enc,
            ciphertext: ct,
            aad_version: 1,
        },
        &auth.actor(),
    )
    .await?;
    pdb::append_audit(
        &pool,
        &AuditEntry {
            actor: &auth.actor(),
            action: "credential_set",
            target: &id,
            reason: None,
            detail: json!({"key_id": c.key_id}),
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn clear_credential(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    Path(id): Path<String>,
) -> R<StatusCode> {
    auth.require_admin()?;
    if !pdb::clear_credential(&pool, &id).await? {
        return Err(ProvidersApiError::NotFound);
    }
    pdb::append_audit(
        &pool,
        &AuditEntry {
            actor: &auth.actor(),
            action: "credential_clear",
            target: &id,
            reason: None,
            detail: json!({}),
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct BenchBody {
    result: String,
    note: Option<String>,
}

async fn record_bench(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    Path(id): Path<String>,
    b: Result<Json<BenchBody>, JsonRejection>,
) -> R<StatusCode> {
    auth.require_admin()?;
    let bb = body(b, "{result: passed|failed, note?}")?;
    if bb.result != "passed" && bb.result != "failed" {
        return Err(ProvidersApiError::BadRequest(
            "result must be passed or failed".into(),
        ));
    }
    if bb.note.as_deref().map(|n| n.chars().count()).unwrap_or(0) > 500 {
        return Err(ProvidersApiError::BadRequest(
            "note must be at most 500 characters".into(),
        ));
    }
    let Some(p) = pdb::get(&pool, &id).await? else {
        return Err(ProvidersApiError::NotFound);
    };
    if !pdb::bench_required(&p.row.kind) {
        return Err(ProvidersApiError::BadRequest(
            "this provider has no bench gate".into(),
        ));
    }
    pdb::set_bench(&pool, &id, &bb.result, bb.note.as_deref(), &auth.actor()).await?;
    pdb::append_audit(
        &pool,
        &AuditEntry {
            actor: &auth.actor(),
            action: "bench_record",
            target: &id,
            reason: bb.note.as_deref(),
            detail: json!({"result": bb.result}),
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RequestBody {
    kind: String,
    zone: Option<String>,
    reason: Option<String>,
}

async fn create_request(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    Path(id): Path<String>,
    b: Result<Json<RequestBody>, JsonRejection>,
) -> R<(StatusCode, Json<serde_json::Value>)> {
    auth.require_admin()?;
    let r = body(b, "{kind: test_connection, zone?, reason?}")?;
    if r.kind != "test_connection" {
        return Err(ProvidersApiError::BadRequest(
            "only test_connection is available; test boot arrives with P-B".into(),
        ));
    }
    let Some(p) = pdb::get(&pool, &id).await? else {
        return Err(ProvidersApiError::NotFound);
    };
    if p.credential.is_none() {
        return Err(ProvidersApiError::BadRequest(
            "enter a token before testing the connection".into(),
        ));
    }
    if rq::count_queued_for(&pool, &id, "test_connection").await? > 0 {
        return Err(ProvidersApiError::Conflict {
            code: "MM_FLEET_REQUEST_PENDING",
            message: "a test connection for this provider is already queued".into(),
        });
    }
    let rid = rq::enqueue(
        &pool,
        &rq::NewRequest {
            kind: "test_connection",
            provider_id: &id,
            zone: r.zone.as_deref(),
            role: None,
            reason: r.reason.as_deref(),
            requested_by: &auth.actor(),
        },
    )
    .await?;
    pdb::append_audit(
        &pool,
        &AuditEntry {
            actor: &auth.actor(),
            action: "request_create",
            target: &id,
            reason: r.reason.as_deref(),
            detail: json!({"kind": "test_connection", "request_id": rid}),
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"id": rid}))))
}

async fn get_request(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    Path(id): Path<String>,
) -> R<Json<rq::RequestRow>> {
    auth.require_admin()?;
    rq::get(&pool, &id)
        .await?
        .map(Json)
        .ok_or(ProvidersApiError::NotFound)
}
