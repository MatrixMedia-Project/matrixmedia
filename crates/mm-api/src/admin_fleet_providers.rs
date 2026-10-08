//! GPU provider profiles, priority, write-only credentials, bench results and operator
//! requests (spec §8.3, P-A subset). mm-core stores ciphertext it cannot read and never
//! returns it; the runner (a separate process) does the opening and the checking.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRef, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use mm_core::config::FleetMode;
use mm_core::config_handle::ConfigHandle;
use mm_fleet::control_db;
use mm_fleet::desired::{DesiredStore, TeardownTarget};
use mm_fleet::endpoint::ip_is_forbidden;
use mm_fleet::nodes_db;
use mm_fleet::placement::{self, Limits, PlacementRequest, Skip};
use mm_fleet::placement_db;
use mm_fleet::providers_db::{
    self as pdb, AuditEntry, CredentialBlob, CredentialSummary, KINDS, ProviderFull, ProviderInput,
    REGIONS, ROLES, StatusRow, ZoneRow,
};
use mm_fleet::requests_db as rq;
use mm_fleet::roles::{Backend, Purpose, Role};
use mm_fleet::test_boot;
use mm_fleet::test_boot_db::{self, NewTestBoot, TestBootRefused};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use url::Host;

use crate::error::ApiError;
use crate::middleware::AdminAuth;

/// The routes' state. `FromRef` lets every P-A handler keep `State<PgPool>`.
#[derive(Clone)]
pub struct FleetApi {
    pub pool: PgPool,
    pub config: ConfigHandle,
}

impl FromRef<FleetApi> for PgPool {
    fn from_ref(s: &FleetApi) -> PgPool {
        s.pool.clone()
    }
}

impl FromRef<FleetApi> for ConfigHandle {
    fn from_ref(s: &FleetApi) -> ConfigHandle {
        s.config.clone()
    }
}

pub fn routes(pool: PgPool, config: ConfigHandle) -> Router {
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
        .route("/broadcast-servers/gpu-nodes", get(gpu_nodes))
        .route("/broadcast-servers/nodes/{id}/drain", post(drain_node))
        .with_state(FleetApi { pool, config })
}

// ---- errors ---------------------------------------------------------------------------

pub enum ProvidersApiError {
    BadRequest(String),
    NotFound,
    Conflict {
        code: &'static str,
        message: String,
    },
    Admin(ApiError),
    Db(sqlx::Error),
    /// A failure the operator cannot act on: the detail is logged, never returned.
    Internal(String),
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
            ProvidersApiError::Internal(detail) => {
                tracing::error!(detail = %detail, "fleet providers internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "MM_INTERNAL", "message": "internal error"})),
                )
                    .into_response()
            }
        }
    }
}

type R<T> = Result<T, ProvidersApiError>;

fn conflict(code: &'static str, message: impl Into<String>) -> ProvidersApiError {
    ProvidersApiError::Conflict {
        code,
        message: message.into(),
    }
}

fn bad(message: impl Into<String>) -> ProvidersApiError {
    ProvidersApiError::BadRequest(message.into())
}

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
    // The settings the runner acts on, from its heartbeat. Every one of these serializes as
    // `null` when unknown, never omitted: the page reads a missing key and a null differently
    // from a value, and an omitted key would hide the difference from its type.
    pub default_region: Option<String>,
    pub create_backend_transcode: Option<String>,
    pub create_backend_fanout: Option<String>,
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
    /// The currency this kind's list prices are in.
    pub currency: &'static str,
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
            // A setting the runner did not report, or reported as something other than text,
            // is unknown: never coerced.
            let setting = |key: &str| r.detail["settings"][key].as_str().map(str::to_string);
            let (default_region, create_backend_transcode, create_backend_fanout) = (
                setting("default_region"),
                setting("create_backend_transcode"),
                setting("create_backend_fanout"),
            );
            (
                RunnerView {
                    reporting: true,
                    heartbeat_at: Some(r.heartbeat_at),
                    version: Some(r.runner_version),
                    key_fingerprint: Some(r.key_fingerprint),
                    public_key_hex: Some(hex::encode(&r.public_key)),
                    fleet_mode_seen: Some(r.fleet_mode_seen),
                    rented_nodes: rented,
                    default_region,
                    create_backend_transcode,
                    create_backend_fanout,
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
                default_region: None,
                create_backend_transcode: None,
                create_backend_fanout: None,
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
                default_region: None,
                create_backend_transcode: None,
                create_backend_fanout: None,
            },
            false,
        ),
    }
}

/// What the demo role may see of a status: the verdict and when it was reached, not the
/// provider-account values (balance, quota, stock, prices, the provider's own error text).
///
/// Every field is named on purpose (no `..s`): a field added to `StatusRow` later must fail to
/// compile here until someone decides whether the demo role may see it, instead of reaching it
/// by default.
fn demo_status(s: StatusRow) -> StatusRow {
    StatusRow {
        provider_id: s.provider_id,
        checked_at: s.checked_at,
        state: s.state,
        key_scope: None,
        quota: json!({}),
        stock: json!({}),
        prices: json!({}),
        balance_minor: None,
        last_error: None,
        last_error_kind: s.last_error_kind,
        last_error_at: s.last_error_at,
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
        currency: pdb::price_currency(&r.kind),
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
    let Some(edited) = update_unless_in_use(&pool, &id, &input).await? else {
        return Err(ProvidersApiError::NotFound);
    };
    pdb::append_audit(
        &pool,
        &AuditEntry {
            actor: &auth.actor(),
            action: "provider_update",
            target: &id,
            reason: None,
            detail: json!({
                "label": input.label,
                "endpoint_changed": edited.endpoint_changed,
            }),
        },
    )
    .await?;
    // 204, like every other write here: the dashboard client reads any other success
    // status as a JSON body, and a save has none to give.
    Ok(StatusCode::NO_CONTENT)
}

// ---- the in-use guards (owner decision Q4) -------------------------------------------------
//
// While a server exists on a provider, the runner must still be able to destroy it through that
// provider: with the same token, the same endpoint, the same account and a zone that still
// exists. So while live nodes reference a provider, an edit of its endpoint or account, the
// removal of a zone one of them sits in, and clearing its token are refused (409
// `MM_FLEET_PROVIDER_IN_USE`). Replacing the token stays allowed: the account binding keeps
// a replacement on the same project.
//
// Each guard runs inside the transaction that makes the change, after that transaction has
// locked the provider row `FOR UPDATE`. A machine's insert (`nodes_db::insert_for_create`) takes
// the same row `FOR NO KEY UPDATE`, so it is either committed before the lock is granted, and
// the guard counts it, or it waits for the change to commit and then finds the new endpoint
// or zones, never a state the guard did not see.

/// Refuses while live nodes reference this provider; `what` names what the change would take
/// from them. Call it inside the change's transaction, after locking the provider row.
async fn ensure_not_in_use(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &str,
    what: &str,
) -> R<()> {
    let n = pdb::live_nodes_for(&mut **tx, id).await?;
    if n > 0 {
        return Err(conflict(
            "MM_FLEET_PROVIDER_IN_USE",
            format!(
                "{n} server(s) are running on this provider; release them before changing its {what}"
            ),
        ));
    }
    Ok(())
}

/// What an edit did, as read under the provider row's lock.
struct Edited {
    endpoint_changed: bool,
}

/// Saves a provider's profile unless that would take away what its live servers need. `None`
/// when there is no live provider with this id.
///
/// This writes what [`pdb::update`] writes, in the same statements: that function (and its
/// zone writer) owns its own transaction, so the guard could not run inside it. Moving the guard
/// into `providers_db` would remove this copy; `a_guarded_edit_writes_what_providers_db_writes`
/// holds the two together until then.
async fn update_unless_in_use(pool: &PgPool, id: &str, input: &ProviderInput) -> R<Option<Edited>> {
    let mut tx = pool.begin().await?;
    let locked: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT endpoint_display, account_display FROM mm_fleet_providers
          WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((endpoint, account)) = locked else {
        tx.rollback().await?;
        return Ok(None);
    };
    let endpoint_changed = endpoint != input.endpoint_display;
    if endpoint_changed || account != input.account_display {
        ensure_not_in_use(&mut tx, id, "endpoint or account").await?;
    }
    let kept: std::collections::HashSet<&str> =
        input.zones.iter().map(|z| z.zone.as_str()).collect();
    if pdb::live_node_zones(&mut *tx, id)
        .await?
        .iter()
        .any(|z| !kept.contains(z.as_str()))
    {
        return Err(conflict(
            "MM_FLEET_PROVIDER_IN_USE",
            "servers are running in a zone this change removes; release them first",
        ));
    }
    sqlx::query(
        "UPDATE mm_fleet_providers SET label=$2, enabled=$3, endpoint_display=$4, account_display=$5, image=$6, gpu_image=$7,
                transcode_image=$8, max_gpu_nodes=$9, updated_at=now() WHERE id=$1 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(&input.label)
    .bind(input.enabled)
    .bind(&input.endpoint_display)
    .bind(&input.account_display)
    .bind(&input.image)
    .bind(&input.gpu_image)
    .bind(&input.transcode_image)
    .bind(input.max_gpu_nodes)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM mm_fleet_provider_zones WHERE provider_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for (i, z) in input.zones.iter().enumerate() {
        sqlx::query("INSERT INTO mm_fleet_provider_zones (provider_id, zone, region, position) VALUES ($1, $2, $3, $4)")
            .bind(id).bind(&z.zone).bind(&z.region).bind(i as i32).execute(&mut *tx).await?;
        for (role, size) in &z.sizes {
            sqlx::query("INSERT INTO mm_fleet_provider_sizes (provider_id, zone, role, size) VALUES ($1, $2, $3, $4)")
                .bind(id).bind(&z.zone).bind(role).bind(size).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(Some(Edited { endpoint_changed }))
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
    // Re-checked under the provider row lock: a delete that landed after the check at the top
    // must not leave a token behind.
    let stored = pdb::put_credential(
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
    if !stored {
        return Err(ProvidersApiError::NotFound);
    }
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

/// Removes the sealed token unless live servers still need it to be destroyed. `false` when
/// there is no live provider, or it has no token. The writes are [`pdb::clear_credential`]'s,
/// run under the provider row's lock so the guard cannot miss a machine (see above).
async fn clear_credential_unless_in_use(pool: &PgPool, id: &str) -> R<bool> {
    let mut tx = pool.begin().await?;
    let live: Option<String> = sqlx::query_scalar(
        "SELECT id FROM mm_fleet_providers WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if live.is_none() {
        tx.rollback().await?;
        return Ok(false);
    }
    ensure_not_in_use(&mut tx, id, "token").await?;
    let cleared = sqlx::query("DELETE FROM mm_fleet_provider_credentials WHERE provider_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query("UPDATE mm_fleet_providers SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(cleared == 1)
}

async fn clear_credential(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    Path(id): Path<String>,
) -> R<StatusCode> {
    auth.require_admin()?;
    if !clear_credential_unless_in_use(&pool, &id).await? {
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
    /// A test boot's typed confirmation.
    confirmation: Option<String>,
}

async fn create_request(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    State(config): State<ConfigHandle>,
    Path(id): Path<String>,
    b: Result<Json<RequestBody>, JsonRejection>,
) -> R<(StatusCode, Json<serde_json::Value>)> {
    auth.require_admin()?;
    let r = body(
        b,
        "{kind: test_connection|test_boot, zone?, reason?, confirmation?}",
    )?;
    match r.kind.as_str() {
        "test_connection" => test_connection_request(&auth, &pool, &id, r).await,
        "test_boot" => test_boot_request(&auth, &pool, &config, &id, r).await,
        _ => Err(bad("kind must be test_connection or test_boot")),
    }
}

async fn test_connection_request(
    auth: &AdminAuth,
    pool: &PgPool,
    id: &str,
    r: RequestBody,
) -> R<(StatusCode, Json<serde_json::Value>)> {
    let Some(p) = pdb::get(pool, id).await? else {
        return Err(ProvidersApiError::NotFound);
    };
    if p.credential.is_none() {
        return Err(bad("enter a token before testing the connection"));
    }
    if rq::count_queued_for(pool, id, "test_connection").await? > 0 {
        return Err(conflict(
            "MM_FLEET_REQUEST_PENDING",
            "a test connection for this provider is already queued",
        ));
    }
    let rid = rq::enqueue(
        pool,
        &rq::NewRequest {
            kind: "test_connection",
            provider_id: id,
            zone: r.zone.as_deref(),
            role: None,
            reason: r.reason.as_deref(),
            requested_by: &auth.actor(),
            params: json!({}),
        },
    )
    .await?;
    pdb::append_audit(
        pool,
        &AuditEntry {
            actor: &auth.actor(),
            action: "request_create",
            target: id,
            reason: r.reason.as_deref(),
            detail: json!({"kind": "test_connection", "request_id": rid}),
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"id": rid}))))
}

/// The phrase the operator types to start a test boot. It rents a GPU, so the request must
/// say so in words that cannot be sent by accident.
const TEST_BOOT_CONFIRMATION: &str = "test boot";

/// Why placement refused a pinned test boot, as the operator should hear it. `None` for the two
/// cap refusals: `test_boot_db::create` decides those exactly, under its locks, and names the
/// limit.
fn test_boot_skip(skip: Skip) -> Option<ProvidersApiError> {
    Some(match skip {
        Skip::ProviderCap | Skip::GlobalCap => return None,
        Skip::NotVerified => conflict(
            "MM_FLEET_PROVIDER_NOT_VERIFIED",
            "run Test connection first: this token is not verified",
        ),
        Skip::NoCredential => bad("enter a token before a test boot"),
        Skip::NoAdapter => bad("test boots are not available for this provider yet"),
        Skip::NoSuchProvider => ProvidersApiError::NotFound,
        Skip::NoSuchZone => bad("that zone is not configured for this provider"),
        Skip::NoSizeForRole => bad("that zone has no GPU size; add a transcode size first"),
        other => ProvidersApiError::Internal(format!(
            "placement refused a pinned test boot for a reason it should not: {}",
            other.as_str()
        )),
    })
}

async fn test_boot_request(
    auth: &AdminAuth,
    pool: &PgPool,
    config: &ConfigHandle,
    id: &str,
    r: RequestBody,
) -> R<(StatusCode, Json<serde_json::Value>)> {
    if r.confirmation.as_deref() != Some(TEST_BOOT_CONFIRMATION) {
        return Err(bad(format!(
            "type \"{TEST_BOOT_CONFIRMATION}\" to confirm: it rents a GPU for up to 15 minutes"
        )));
    }
    let reason = r
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("give a reason"))?;
    if reason.chars().count() > 500 {
        return Err(bad("the reason is at most 500 characters"));
    }
    let zone = r.zone.as_deref().ok_or_else(|| bad("pick a zone"))?;
    let Some(p) = pdb::get(pool, id).await? else {
        return Err(ProvidersApiError::NotFound);
    };
    let z = p
        .zones
        .iter()
        .find(|z| z.zone == zone)
        .ok_or_else(|| bad("that zone is not configured for this provider"))?;
    let size = z
        .sizes
        .get("transcode")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| bad("that zone has no GPU size; add a transcode size first"))?;
    if p.credential.is_none() {
        return Err(bad("enter a token before a test boot"));
    }

    let cfg = config.load();
    if cfg.fleet.mode == FleetMode::Off {
        return Err(conflict(
            "MM_FLEET_OFF",
            "fleet.mode is off: nothing may be rented, test boots included",
        ));
    }
    let now = Utc::now();
    let reporting = control_db::read(pool)
        .await?
        .is_some_and(|c| !control_db::is_stale(&c, now));
    if !reporting {
        return Err(conflict(
            "MM_FLEET_RUNNER_NOT_REPORTING",
            "the runner is not reporting; a test boot needs it",
        ));
    }
    let public = cfg.server.public_url.as_deref().ok_or_else(|| {
        bad("server.public_url is not set: the test machine would have nowhere to report")
    })?;
    // The runner's rules rest on this URL; a setting it cannot use is the server's to fix, not
    // something wrong with the request. The message names the rule, never the setting's value.
    let url = test_boot::report_url(public)
        .map_err(|rule| conflict("MM_FLEET_PUBLIC_URL_INVALID", rule))?;

    // The placement rules for a pinned test boot decide whether this token is verified (a Test
    // connection `ok`, newer than the token, within CHECK_FRESH_SECS) and whether an adapter
    // exists: the same rules the runner applies before it rents. Read here, right before the
    // request is queued. A token replaced between this and `create` is the runner's to catch:
    // it re-checks before any create.
    let (facts, gpu_nodes_live) = placement_db::load_facts(pool).await?;
    let pinned = placement::pinned(
        &facts,
        id,
        zone,
        &PlacementRequest {
            role: Role::Transcode,
            region: z.region.clone(),
            purpose: Purpose::TestBoot,
            backend: Backend::Api,
            now,
        },
        &Limits {
            max_gpu_nodes: cfg.fleet.max_gpu_nodes,
            gpu_nodes_live,
        },
    );
    if let Err(skip) = pinned
        && let Some(refusal) = test_boot_skip(skip)
    {
        return Err(refusal);
    }

    let actor = auth.actor();
    match test_boot_db::create(
        pool,
        &NewTestBoot {
            provider_id: id,
            zone,
            region: &z.region,
            size,
            reason,
            requested_by: &actor,
            report_url: &url,
            per_day: cfg.fleet.test_boots_per_day,
            global_cap: cfg.fleet.max_gpu_nodes,
        },
    )
    .await
    {
        Ok((rid, node)) => {
            pdb::append_audit(
                pool,
                &AuditEntry {
                    actor: &actor,
                    action: "request_create",
                    target: id,
                    reason: Some(reason),
                    detail: json!({"kind": "test_boot", "request_id": rid, "node_id": node.as_str(), "zone": zone, "size": size}),
                },
            )
            .await?;
            Ok((StatusCode::ACCEPTED, Json(json!({"id": rid}))))
        }
        Err(TestBootRefused::AlreadyRunning) => Err(conflict(
            "MM_FLEET_TEST_BOOT_RUNNING",
            "a test boot is already running; wait for it to finish",
        )),
        Err(e @ TestBootRefused::DailyLimit { .. }) => {
            Err(conflict("MM_FLEET_TEST_BOOT_LIMIT", e.to_string()))
        }
        Err(e @ (TestBootRefused::GlobalCap { .. } | TestBootRefused::ProviderCap { .. })) => {
            Err(conflict("MM_FLEET_GPU_CAP", e.to_string()))
        }
        Err(TestBootRefused::ProviderGone) => Err(ProvidersApiError::NotFound),
        Err(TestBootRefused::Db(e)) => Err(e.into()),
    }
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

// ---- running GPU servers ----------------------------------------------------------------

/// A rented GPU server that is not yet gone, as the Running GPU servers card shows it. For the
/// demo role the account-level and personal fields are `null`: who started it, what the boot
/// probe saw, which broadcast it serves, and what the provider charges for it.
#[derive(Serialize)]
pub struct GpuNodeView {
    pub id: String,
    pub provider_id: Option<String>,
    pub provider_label: Option<String>,
    pub kind: Option<String>,
    pub zone: Option<String>,
    pub size: Option<String>,
    pub purpose: String,
    pub broadcast_id: Option<String>,
    pub state: String,
    pub created_by: Option<String>,
    pub billing_started_at: Option<DateTime<Utc>>,
    pub destroy_deadline: Option<DateTime<Utc>>,
    pub price_per_hour: Option<f64>,
    pub currency: Option<&'static str>,
    pub est_cost: Option<f64>,
    pub request_id: Option<String>,
    pub boot_report: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct TestBootQuota {
    pub per_day: i64,
    pub used_today: i64,
    pub left_today: i64,
}

#[derive(Serialize)]
pub struct GpuNodesResponse {
    pub demo: bool,
    pub nodes: Vec<GpuNodeView>,
    pub test_boots: TestBootQuota,
    pub max_gpu_nodes: i64,
    pub transcode_software_configured: bool,
}

/// `bc-<broadcast>-transcode-<n>` names its broadcast. The desired row says so directly, but it
/// is gone once a release has ordered the teardown, and the node must still be attributable.
fn broadcast_of(node_id: &str) -> Option<String> {
    let rest = node_id.strip_prefix("bc-")?;
    let (head, ordinal) = rest.rsplit_once('-')?;
    ordinal.parse::<u32>().ok()?;
    head.strip_suffix("-transcode").map(str::to_string)
}

/// The node's stored boot report, only if it is still a valid one. What the probe sent is
/// untrusted: a stored value that no longer validates reads as no report, and what is served is
/// the typed report re-serialized, never the stored JSON as it lies in the row.
fn boot_report_view(stored: &serde_json::Value) -> Option<serde_json::Value> {
    let report = test_boot::report_of(stored)?;
    let received_at = stored
        .get("received_at")
        .and_then(|t| serde_json::from_value::<DateTime<Utc>>(t.clone()).ok());
    Some(match received_at {
        Some(at) => test_boot::stored_report(&report, at),
        None => json!({ "report": report }),
    })
}

async fn gpu_nodes(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    State(config): State<ConfigHandle>,
) -> R<Json<GpuNodesResponse>> {
    let demo = auth.is_demo();
    let now = Utc::now();
    let nodes = nodes_db::gpu_nodes_view(&pool)
        .await?
        .into_iter()
        .map(|n| {
            let price = n
                .prices
                .as_ref()
                .zip(n.size.as_ref())
                .and_then(|(prices, size)| prices.get(size))
                .and_then(|v| v.as_f64());
            let est = n.billing_started_at.and_then(|started| {
                test_boot::estimate_cost(price, test_boot::billed_minutes(started, now))
            });
            GpuNodeView {
                request_id: test_boot::request_id_for(&n.mm_node_id),
                broadcast_id: if demo {
                    None
                } else {
                    n.broadcast_id
                        .clone()
                        .or_else(|| broadcast_of(&n.mm_node_id))
                },
                currency: n.kind.as_deref().map(pdb::price_currency),
                id: n.mm_node_id,
                provider_id: n.provider_ref,
                provider_label: n.provider_label,
                kind: n.kind,
                zone: n.provider_zone,
                size: n.size,
                purpose: n.purpose,
                state: n.state,
                created_by: if demo { None } else { n.created_by },
                billing_started_at: n.billing_started_at,
                destroy_deadline: n.destroy_deadline,
                price_per_hour: if demo { None } else { price },
                est_cost: if demo { None } else { est },
                boot_report: if demo {
                    None
                } else {
                    n.boot_report.as_ref().and_then(boot_report_view)
                },
            }
        })
        .collect();
    let cfg = config.load();
    let used = rq::count_today(&pool, "test_boot").await?;
    let software = pdb::list(&pool).await?.iter().any(|p| {
        p.row.enabled
            && p.row
                .transcode_image
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
    });
    Ok(Json(GpuNodesResponse {
        demo,
        nodes,
        test_boots: TestBootQuota {
            per_day: cfg.fleet.test_boots_per_day,
            used_today: used,
            left_today: (cfg.fleet.test_boots_per_day - used).max(0),
        },
        max_gpu_nodes: cfg.fleet.max_gpu_nodes,
        transcode_software_configured: software,
    }))
}

// ---- release ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct DrainBody {
    reason: String,
}

/// Releases a rented GPU server: mm-core orders its teardown and the runner destroys it. This
/// handler never calls a provider (owner decision Q1): the order is a database write, the
/// desired row deleted and the node marked `destroying` in one transaction, and the runner
/// completes it, and the sweeper backs it up past the node's deadline.
async fn drain_node(
    auth: AdminAuth,
    State(pool): State<PgPool>,
    Path(id): Path<String>,
    b: Result<Json<DrainBody>, JsonRejection>,
) -> R<StatusCode> {
    auth.require_admin()?;
    let DrainBody { reason } = body(b, "{reason}")?;
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > 500 {
        return Err(bad("give a reason of 1 to 500 characters"));
    }
    let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT flavor, ownership, state, purpose, provider_id FROM mm_fleet_nodes WHERE mm_node_id = $1",
    )
    .bind(&id)
    .fetch_optional(&pool)
    .await?;
    let Some((flavor, ownership, state, purpose, provider_id)) = row else {
        return Err(ProvidersApiError::NotFound);
    };
    if flavor != "transcode" || ownership != "rented" {
        return Err(bad(
            "only rented GPU servers can be released here; draining fan-out servers arrives with the server inventory",
        ));
    }
    if state == "gone" || state == "destroying" {
        return Err(conflict(
            "MM_FLEET_ALREADY_RELEASED",
            "this server is already being destroyed",
        ));
    }
    let broadcast = sqlx::query_scalar::<_, Option<String>>(
        "SELECT broadcast_id FROM mm_fleet_desired WHERE mm_node_id = $1",
    )
    .bind(&id)
    .fetch_optional(&pool)
    .await?
    .flatten()
    .or_else(|| broadcast_of(&id));
    if purpose == "broadcast"
        && let Some(bc) = &broadcast
    {
        // FR-314c: sticky, so the planner does not order another; only the broadcaster can
        // undo it. Before the teardown order, so no tick can plan a replacement in between.
        mm_db::transcode_db::release(&pool, bc).await?;
    }
    DesiredStore::new(pool.clone())
        .order_teardown(&TeardownTarget {
            mm_node_id: mm_core::fleet::NodeId::new(&id),
            ownership: mm_core::fleet::Ownership::Rented,
            flavor: mm_core::fleet::NodeFlavor::Transcode,
            provider_id,
        })
        .await
        .map_err(|e| ProvidersApiError::Internal(e.to_string()))?;
    let actor = auth.actor();
    if purpose == "test_boot"
        && let Some(rid) = test_boot::request_id_for(&id)
    {
        rq::progress(
            &pool,
            &rid,
            json!({"released_by": actor, "released_reason": reason}),
        )
        .await?;
    }
    pdb::append_audit(
        &pool,
        &AuditEntry {
            actor: &actor,
            action: "release_rented",
            target: &id,
            reason: Some(reason),
            detail: json!({"purpose": purpose, "broadcast_id": broadcast}),
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
