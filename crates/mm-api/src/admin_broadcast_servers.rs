//! `GET /_mm/admin/v1/broadcast-servers` — the collector's last snapshot.
//!
//! Served from memory: a request never probes a server, so open dashboards add no load.
//! The demo role gets the page's structure only — decided here by role, never by
//! inspecting values.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::broadcast_servers::{BroadcastServersView, SnapshotCell};
use crate::middleware::AdminAuth;

pub fn routes(cell: Arc<SnapshotCell>) -> Router {
    Router::new()
        .route("/broadcast-servers", get(get_broadcast_servers))
        .with_state(cell)
}

async fn get_broadcast_servers(
    auth: AdminAuth,
    State(cell): State<Arc<SnapshotCell>>,
) -> Json<BroadcastServersView> {
    Json(view_for(auth.is_demo(), &cell))
}

/// What a caller may see. Demo: structure only, whatever the cell holds.
pub fn view_for(demo: bool, cell: &SnapshotCell) -> BroadcastServersView {
    if demo {
        return BroadcastServersView::demo();
    }
    cell.get().unwrap_or_else(BroadcastServersView::collecting)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broadcast_servers::fixtures::sample;
    use crate::broadcast_servers::{Trackers, build_view};

    fn populated() -> SnapshotCell {
        let cell = SnapshotCell::new();
        cell.set(build_view(&sample(), &mut Trackers::default()));
        cell
    }

    #[test]
    fn demo_gets_structure_whatever_the_cell_holds() {
        let j = serde_json::to_string(&view_for(true, &populated())).unwrap();
        assert!(j.contains("\"demo\":true"), "{j}");
        for leaked in ["Secret title", "leak.example", "3f2a1c9e", "\"ok\""] {
            assert!(!j.contains(leaked), "demo leaked {leaked}: {j}");
        }
    }

    #[test]
    fn admin_gets_the_snapshot_without_viewer_ids() {
        let j = serde_json::to_string(&view_for(false, &populated())).unwrap();
        assert!(j.contains("Secret title"), "{j}");
        // The sample's viewer "@viewer:leak.example" has the switch id
        // "viewer-<stream>-viewer-leak.example": neither its prefix nor its user part may appear.
        assert!(
            !j.contains("viewer-") && !j.contains("-viewer-leak.example"),
            "{j}"
        );
    }

    #[test]
    fn before_the_first_tick_the_admin_sees_collecting() {
        let v = view_for(false, &SnapshotCell::new());
        assert!(!v.demo);
        assert!(v.collected_at.is_none());
    }
}
