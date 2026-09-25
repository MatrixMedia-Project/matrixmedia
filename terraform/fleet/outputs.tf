# What mm-core reads back after an apply, to populate mm_fleet_nodes.
#
# Keyed on mm_node_id so the reconciler can join without guessing. A node that
# failed to create is absent rather than present-with-nulls: mm-core must be able
# to tell "not created" from "created without an address", because only the second
# is a machine that may be billing.
output "node_ids" {
  description = "mm_node_id => provider instance id"
  value       = { for id, n in terraform_data.node : id => n.id }
}

output "node_addresses" {
  description = "mm_node_id => public IP. Empty until a real provider replaces terraform_data."
  value       = {}
}

output "node_count" {
  description = "How many nodes this state believes exist — compared against mm_fleet_nodes to find drift."
  value       = length(terraform_data.node)
}
