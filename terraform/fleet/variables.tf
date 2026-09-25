# The desired set, written by mm-core as desired_nodes.auto.tfvars.json.
#
# The map KEY is mm_node_id and it is the `for_each` key, so it is also the
# Terraform resource address. Two consequences worth stating where they will be
# read:
#
#   * every key missing from the file is a machine Terraform DESTROYS;
#   * changing a key renames the address, which destroys and recreates.
#
# mm-core therefore never reuses an mm_node_id, including one belonging to a node
# it has already destroyed (planner::next_free_ordinal).
#
# The attribute names here are the contract with mm-fleet's `TfNode`. Renaming one
# on either side without the other destroys and recreates the whole fleet.
variable "desired_nodes" {
  description = "mm_node_id => node spec, rendered by mm-core's fleet runner"
  type = map(object({
    flavor    = string
    ownership = string
    region    = string
    size      = string
    # Optional in the JSON, so both carry a default: `broadcast_id` is absent for
    # a node not tied to one broadcast, and `destroy_deadline` is absent for any
    # node that is not rented.
    broadcast_id     = optional(string)
    destroy_deadline = optional(string)
  }))
  default = {}

  validation {
    condition = alltrue([
      for k, v in var.desired_nodes :
      contains(["origin", "fanout", "edge", "transcode"], v.flavor)
    ])
    error_message = "flavor must be one of origin, fanout, edge, transcode — the same set as V034's CHECK constraint."
  }

  validation {
    condition = alltrue([
      for k, v in var.desired_nodes :
      contains(["owned", "leased", "rented"], v.ownership)
    ])
    error_message = "ownership must be one of owned, leased, rented — the same set as V034's CHECK constraint."
  }

  # The cost-safety invariant, restated at the boundary that acts on it. mm-core
  # enforces it in the database (desired_rented_needs_deadline) and in the store;
  # this is the third place, and it is the one holding the API token.
  validation {
    condition = alltrue([
      for k, v in var.desired_nodes :
      v.ownership != "rented" || v.destroy_deadline != null
    ])
    error_message = "a rented node MUST carry destroy_deadline: without one nothing bounds what it costs."
  }

  validation {
    condition = alltrue([
      for k, v in var.desired_nodes :
      v.ownership == "rented" || v.destroy_deadline == null
    ])
    error_message = "only a rented node may carry destroy_deadline: a deadline on owned or leased capacity invites the reaper to destroy it."
  }
}

variable "zone" {
  description = "Scaleway zone. COMPUTE3 is in nl-ams-1/-2 and fr-par-1/-2; POP2-HN only in nl-ams-3 and pl-waw-1; L4 GPUs only in fr-par-1 and pl-waw-2. No zone has both COMPUTE3 and POP2-HN."
  type        = string
  default     = "nl-ams-1"
}

variable "region" {
  description = "Scaleway region containing `zone`."
  type        = string
  default     = "nl-ams"
}

variable "image" {
  description = "Image label or local-image UUID for fleet nodes."
  type        = string
  default     = "ubuntu_noble"
}

variable "fleet_tag" {
  description = "Tag marking an instance as ours. The orphan sweeper's only basis for ownership, so it must match mm-fleet's ScalewayProvider::fleet_tag exactly."
  type        = string
  default     = "mm-fleet"
}

variable "mm_switch_auth_secret" {
  description = "HMAC secret every fleet node needs; a fleet node refuses to boot without it (FR-348)."
  type        = string
  sensitive   = true
  default     = ""
}
