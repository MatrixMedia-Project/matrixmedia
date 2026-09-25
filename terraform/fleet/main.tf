# Fleet nodes, one per entry of the desired set.
#
# WHAT IS MISSING: the instance resource itself. Which provider sells hourly
# instances is still an open decision (dev plan §B.0) — the research settled cost
# and the colocation spec settled rent-first, but neither names an API.
#
# `terraform_data` stands in so the module VALIDATES and the for_each wiring,
# the variable shape and the outputs are all real and checkable today. Replacing
# it is one resource block plus a provider block; nothing else in this directory
# changes.
locals {
  # Cloud-init for a fleet node. MM_SWITCH_NODE_FLAVOR is what makes mm-switch
  # refuse to start unauthenticated on a public machine (FR-348), so it is set
  # from the node's own flavor rather than hard-coded.
  user_data = {
    for id, node in var.desired_nodes : id => <<-CLOUDINIT
      #cloud-config
      write_files:
        - path: /etc/mm-switch.env
          permissions: "0600"
          content: |
            MM_SWITCH_NODE_FLAVOR=${node.flavor}
            MM_SWITCH_AUTH_SECRET=${var.mm_switch_auth_secret}
            MM_SWITCH_PRIVATE_VIEWER_LIST=true
            MM_SWITCH_LISTEN=0.0.0.0:7890
    CLOUDINIT
  }
}

resource "terraform_data" "node" {
  for_each = var.desired_nodes

  # Any change here replaces the node. Region and size genuinely require a new
  # machine; flavor does too, because it decides what the node runs. The deadline
  # is deliberately NOT included — extending a node's life must not destroy it.
  triggers_replace = {
    flavor = each.value.flavor
    region = each.value.region
    size   = each.value.size
  }

  input = {
    mm_node_id   = each.key
    flavor       = each.value.flavor
    ownership    = each.value.ownership
    region       = each.value.region
    size         = each.value.size
    broadcast_id = try(each.value.broadcast_id, null)
    user_data    = local.user_data[each.key]
  }
}
