//! Read-only provider checks the runner runs every 5 minutes and on Test connection
//! (spec §6.2). A check never creates anything. What a provider cannot report is `None`,
//! shown as "not available from this provider" — never a made-up value.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::provider::ProviderError;
use crate::providers_db::StatusRow;
use crate::scaleway::ScalewayProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stock {
    Available,
    Scarce,
    Shortage,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Ok,
    NeedsYou,
    Unknown,
}

impl CheckState {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckState::Ok => "ok",
            CheckState::NeedsYou => "needs_you",
            CheckState::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoneReport {
    pub zone: String,
    pub stock: BTreeMap<String, Stock>,
    pub instances_running: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckReport {
    pub state: CheckState,
    pub key_scope: Option<String>,
    pub zones: Vec<ZoneReport>,
    pub prices: BTreeMap<String, f64>,
    pub balance_minor: Option<i64>,
    /// (kind, message). The message is the provider's status line, never a credential.
    pub last_error: Option<(String, String)>,
}

pub fn error_kind(e: &ProviderError) -> &'static str {
    match e {
        ProviderError::Transient(_) => "transient",
        ProviderError::Permanent(_) => "permanent",
        ProviderError::Capacity(_) => "capacity",
        ProviderError::Quota(_) => "quota",
    }
}

impl CheckReport {
    pub fn to_status_row(
        &self,
        provider_id: &str,
        max_gpu_nodes: i32,
        now: DateTime<Utc>,
    ) -> StatusRow {
        let mut quota = serde_json::Map::new();
        let mut stock = serde_json::Map::new();
        for z in &self.zones {
            quota.insert(
                z.zone.clone(),
                json!({"used": z.instances_running, "limit": max_gpu_nodes}),
            );
            stock.insert(
                z.zone.clone(),
                serde_json::to_value(&z.stock).unwrap_or(json!({})),
            );
        }
        StatusRow {
            provider_id: provider_id.to_string(),
            checked_at: now,
            state: self.state.as_str().to_string(),
            key_scope: self.key_scope.clone(),
            quota: quota.into(),
            stock: stock.into(),
            prices: serde_json::to_value(&self.prices).unwrap_or(json!({})),
            balance_minor: self.balance_minor,
            last_error: self.last_error.as_ref().map(|(_, m)| m.clone()),
            last_error_kind: self.last_error.as_ref().map(|(k, _)| k.clone()),
            last_error_at: self.last_error.as_ref().map(|_| now),
        }
    }
}

#[async_trait]
pub trait ProviderChecker: Send + Sync {
    async fn check(&self) -> CheckReport;
}

pub struct ScalewayChecker {
    pub secret_key: String,
    pub project_id: String,
    pub fleet_tag: String,
    pub base_url: String,
    /// (zone, sizes configured for it) in failover order.
    pub zones: Vec<(String, Vec<String>)>,
}

impl std::fmt::Debug for ScalewayChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScalewayChecker")
            .field("project_id", &self.project_id)
            .field("zones", &self.zones.len())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ProviderChecker for ScalewayChecker {
    async fn check(&self) -> CheckReport {
        let mut report = CheckReport {
            state: CheckState::Ok,
            key_scope: None,
            zones: Vec::new(),
            prices: BTreeMap::new(),
            balance_minor: None,
            last_error: None,
        };
        for (zone, sizes) in &self.zones {
            // Image "unused": every call below is a read, so nothing here ever creates.
            let p = ScalewayProvider::new(
                &self.secret_key,
                &self.project_id,
                zone,
                "unused",
                &self.fleet_tag,
            )
            .with_base_url(&self.base_url);
            if let Err(e) = p.verify_key().await {
                report.state = if e.needs_human() {
                    CheckState::NeedsYou
                } else {
                    CheckState::Unknown
                };
                report.last_error = Some((error_kind(&e).into(), e.to_string()));
                report.zones.push(ZoneReport {
                    zone: zone.clone(),
                    stock: BTreeMap::new(),
                    instances_running: None,
                });
                continue;
            }
            let mut zr = ZoneReport {
                zone: zone.clone(),
                stock: BTreeMap::new(),
                instances_running: None,
            };
            match p.availability().await {
                Ok(all) => {
                    for s in sizes {
                        zr.stock
                            .insert(s.clone(), *all.get(s).unwrap_or(&Stock::Unknown));
                    }
                }
                Err(e) => {
                    report.last_error = Some((error_kind(&e).into(), e.to_string()));
                }
            }
            match p.hourly_prices().await {
                Ok(all) => {
                    for s in sizes {
                        if let Some(v) = all.get(s) {
                            report.prices.insert(s.clone(), *v);
                        }
                    }
                }
                Err(e) => {
                    report.last_error = Some((error_kind(&e).into(), e.to_string()));
                }
            }
            match crate::provider::Provider::list(&p).await {
                Ok(handles) => zr.instances_running = Some(handles.len() as u32),
                Err(e) => {
                    report.last_error = Some((error_kind(&e).into(), e.to_string()));
                }
            }
            report.zones.push(zr);
        }
        report
    }
}
