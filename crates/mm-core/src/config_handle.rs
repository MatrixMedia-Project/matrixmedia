//! Atomically swappable configuration snapshot (spec §6.1).
//!
//! Handlers take ONE snapshot per request (`state.config()`) and read everything
//! from it, so a settings save landing mid-request can never mix old and new values.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;

#[derive(Clone)]
pub struct ConfigHandle(Arc<ArcSwap<Config>>);

impl ConfigHandle {
    pub fn new(config: Config) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(config)))
    }

    /// The current snapshot: an atomic load plus a refcount bump.
    pub fn load(&self) -> Arc<Config> {
        self.0.load_full()
    }

    /// Publish a new snapshot. Readers holding an older `Arc` keep their view.
    pub fn store(&self, config: Config) {
        self.0.store(Arc::new(config));
    }
}

impl std::fmt::Debug for ConfigHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfigHandle(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn load_returns_the_latest_store() {
        let h = ConfigHandle::new(Config::default());
        let mut c = Config::default();
        c.server.cors_origins = vec!["https://a.example".into()];
        h.store(c);
        assert_eq!(h.load().server.cors_origins, vec!["https://a.example"]);
    }

    #[test]
    fn a_held_snapshot_is_not_changed_by_a_later_store() {
        let h = ConfigHandle::new(Config::default());
        let before = h.load();
        let mut c = Config::default();
        c.streaming.auto_end_grace_secs = 1;
        h.store(c);
        assert_eq!(
            before.streaming.auto_end_grace_secs,
            Config::default().streaming.auto_end_grace_secs
        );
        assert_eq!(h.load().streaming.auto_end_grace_secs, 1);
    }

    #[test]
    fn clones_share_one_cell() {
        let a = ConfigHandle::new(Config::default());
        let b = a.clone();
        let mut c = Config::default();
        c.recording.retention_days = 7;
        b.store(c);
        assert_eq!(a.load().recording.retention_days, 7);
    }
}
