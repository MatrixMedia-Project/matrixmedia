# What mm-core reads back after an apply, to populate mm_fleet_nodes.
#
# Keyed on mm_node_id so the reconciler can join without guessing.
output "node_ids" {
  description = "mm_node_id => zoned instance id, `zone/uuid` (the provider's `id` attribute, not a bare UUID). ScalewayProvider's create and list return the same form, and the orphan sweeper compares these strings exactly."
  value       = { for id, n in scaleway_instance_server.node : id => n.id }
}

output "node_addresses" {
  description = "mm_node_id => public IPv4. Dynamic, so it changes on every recreate — which is why mm-core learns it from here rather than from configuration."
  # `public_ips` is a LIST of objects, not a single string: a server can hold an
  # IPv4 and an IPv6, and `public_ip` (singular) was removed. Taking the first
  # v4 rather than `[0].address` blindly, because the ordering is not documented
  # and an IPv6-first server would otherwise hand mm-core an address its
  # WebRTC clients cannot reach.
  value = {
    for id, n in scaleway_instance_server.node : id => try(
      [for ip in n.public_ips : ip.address if ip.family == "inet"][0],
      null
    )
  }
}

output "node_count" {
  description = "How many RENTED nodes this state believes exist. Compared against mm_fleet_nodes to find drift — and it will not match the desired-set size when owned or leased nodes are present, by design."
  value       = length(scaleway_instance_server.node)
}
