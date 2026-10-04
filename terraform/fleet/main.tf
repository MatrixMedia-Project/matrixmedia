# Fleet nodes, one per entry of the desired set.
#
# The instance resource is real now that §B.0 is decided (see
# WorkingDirectory/docs/2026-09-25-provider-recommendation.md). Scaleway covers
# fan-out, edge and GPU transcode; OVHcloud is the second implementation, needed
# for US points of presence, and lives in its own directory when it arrives.

locals {
  # Cloud-init for a fleet node. MM_SWITCH_NODE_FLAVOR is what makes mm-switch
  # refuse to start unauthenticated on a public machine (FR-348), so it is set from
  # the node's own flavor rather than hard-coded.
  user_data = {
    for id, node in local.rented_nodes : id => <<-CLOUDINIT
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

# ⚠️ RENTED ONLY.
#
# The desired set contains every node mm-core wants to exist, including the `owned`
# colocated origin and any `leased` monthly boxes. Those exist independently of
# Terraform — they are ITLDC hardware — and handing them to `for_each` would have
# Terraform try to CREATE duplicates of them at Scaleway, and `terraform destroy`
# try to destroy machines it never made.
#
# This is `Ownership::is_reapable` expressed at the fourth and last layer: the
# database CHECK, `DesiredStore::teardown`, the sweeper's selector, and here — the
# one holding the provider API token.
locals {
  rented_nodes = {
    for id, node in var.desired_nodes : id => node
    if node.ownership == "rented"
  }
}

resource "scaleway_instance_server" "node" {
  for_each = local.rented_nodes

  name  = each.key
  type  = each.value.size
  # Transcode nodes need the NVIDIA driver for NVENC; every other flavor boots
  # the plain image. The API path's equivalent, ScalewayProvider::with_gpu_image,
  # has no default and refuses a transcode create without one; here the variable
  # defaults to Scaleway's GPU OS image.
  image = each.value.flavor == "transcode" ? var.gpu_image : var.image

  # ⚠️ EXPLICIT, and the reason is not obvious.
  #
  # A dynamic IP is created with the instance and destroyed with it. A *reserved*
  # IP (`scaleway_instance_ip` + `ip_id`) outlives the server and keeps billing at
  # €0.004/h — forever, and invisibly, because mm-core's orphan sweeper lists
  # INSTANCES, not IPs.
  #
  # This provider defaults `enable_dynamic_ip` to **false**, because Terraform users
  # normally declare a reserved IP. The Instance API defaults the same setting to
  # **true**. The two layers we use disagree, so neither default is safe to rely on
  # and both sides set it explicitly.
  enable_dynamic_ip = true

  # The orphan sweeper's entire basis for telling our machine from someone else's,
  # so it is set at create time rather than added afterwards — the window between
  # the two is exactly when a failure leaves an untraceable billing instance.
  tags = [
    var.fleet_tag,
    "mm-node-id=${each.key}",
    "mm-flavor=${each.value.flavor}",
    "mm-ownership=${each.value.ownership}",
  ]

  user_data = {
    cloud-init = local.user_data[each.key]
  }

  root_volume {
    # Explicit, though it is also the default. COMPUTE3 and BASIC3 report
    # `per_volume_constraint.l_ssd.max_size = 0` — they cannot take local storage at
    # all — so this volume is SBS, and Scaleway's own SDK says `terminate` only
    # DETACHES an sbs_volume. Terraform handles it because it owns the volume as part
    # of this resource; mm-fleet's direct-API teardown has to delete it by hand, and
    # does (see crates/mm-fleet/src/scaleway.rs).
    delete_on_termination = true
  }

  lifecycle {
    # The desired set is authoritative and is rewritten every tick. Without this, a
    # drifted tag or a provider-side default would show up as a diff on every plan
    # and make the real changes — creations and destructions — hard to see in the
    # output a human is supposed to read before approving a destroy.
    ignore_changes = [user_data]
  }
}
