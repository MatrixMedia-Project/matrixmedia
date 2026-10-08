//! Provisioning for the broadcast fleet (WS-B).
//!
//! WS-A gave mm-core the vocabulary (`mm_core::fleet`), the schema (V034), the
//! placement pool and a pure planner. This crate is the part that spends money:
//! it turns a desired set into machines that exist, and — the half that actually
//! matters — machines that stop existing.
//!
//! Two ordering invariants govern everything here, and they are mirror images:
//!
//! 1. **Write `destroy_deadline` before any provider call.** Between "we asked a
//!    provider for a paid machine" and "we recorded that we have one", the
//!    machine already bills. WS-A put the column on `mm_fleet_desired` for this
//!    reason.
//! 2. **Delete the desired row before destroying at the provider.** If the order
//!    reverses and the deletion fails, the next `terraform apply` sees a desired
//!    node with no instance and **creates a new paid machine** — teardown
//!    becomes provisioning. A teardown path that destroys first is wrong even
//!    when it usually works.
//!
//! The second invariant also settles what the design left implicit: the deadline
//! sweeper does **not** actuate through Terraform. A deadline is only ever
//! reached because the primary path failed, and routing the remedy through the
//! mechanism that just failed is how a cost leak becomes permanent.

pub mod checks;
pub mod control_db;
pub mod desired;
pub mod endpoint;
pub mod ladder_billing;
pub mod ladder_loop;
pub mod meter_loop;
pub mod metering;
pub mod placement;
pub mod placement_db;
pub mod provider;
pub mod providers_db;
pub mod rating;
pub mod requests_db;
pub mod roles;
pub mod runner;
pub mod runner_settings;
pub mod scaleway;
pub mod sealed;
pub mod sweeper;
pub mod tfvars;
pub mod wallet_billing;
