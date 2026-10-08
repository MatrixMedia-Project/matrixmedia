//! "Am I still the one runner?" The fleet runner holds a lock that makes it the only process
//! that creates and destroys machines. Code that spends money asks this before each call, so
//! it lives here, where the create path is; the runner supplies the answer from its lock.

/// Asked before every batch of creates or destroys, and before each create call itself.
/// `false` means another process may be the leader now: stop, and leave what exists recorded.
#[async_trait::async_trait]
pub trait LeaderCheck: Send + Sync {
    async fn still_leader(&self) -> bool;
}

/// For tests that exercise the loops without a lock.
pub struct AlwaysLeader;

#[async_trait::async_trait]
impl LeaderCheck for AlwaysLeader {
    async fn still_leader(&self) -> bool {
        true
    }
}
