//! The fleet's small vocabularies, shared by placement, rental and the settings the runner reads.
//! String forms match the CHECKs in V041 (`mm_fleet_provider_sizes.role`, `purpose`,
//! `created_backend`).

use mm_core::fleet::NodeFlavor;

/// What a rented node is for, as provider sizes are keyed (`mm_fleet_provider_sizes.role`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Fanout,
    Edge,
    Transcode,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Fanout => "fanout",
            Role::Edge => "edge",
            Role::Transcode => "transcode",
        }
    }

    /// The role a node flavor is rented as; `None` for the origin, which is never rented.
    pub fn from_flavor(f: NodeFlavor) -> Option<Role> {
        match f {
            NodeFlavor::Fanout => Some(Role::Fanout),
            NodeFlavor::Edge => Some(Role::Edge),
            NodeFlavor::Transcode => Some(Role::Transcode),
            NodeFlavor::Origin => None,
        }
    }
}

/// Why a node exists (`mm_fleet_nodes.purpose`, `mm_fleet_desired.purpose`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Broadcast,
    TestBoot,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Broadcast => "broadcast",
            Purpose::TestBoot => "test_boot",
        }
    }

    pub fn parse(s: &str) -> Option<Purpose> {
        match s {
            "broadcast" => Some(Purpose::Broadcast),
            "test_boot" => Some(Purpose::TestBoot),
            _ => None,
        }
    }
}

/// How machines of a role are created (spec D-C6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The runner calls the provider API.
    Api,
    /// The runner writes the desired set to tfvars; a human applies it.
    Terraform,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Api => "api",
            Backend::Terraform => "terraform",
        }
    }

    pub fn parse(s: &str) -> Option<Backend> {
        match s {
            "api" => Some(Backend::Api),
            "terraform" => Some(Backend::Terraform),
            _ => None,
        }
    }
}
