# terraform/fleet

Turns mm-core's desired set into machines. Consumes `desired_nodes.auto.tfvars.json`,
which the fleet runner writes (`mm_fleet::tfvars`).

## Not finished, and why

**The instance resource is a placeholder.** Which provider sells hourly instances
is an open decision — dev plan §B.0. `terraform_data` stands in so this module
validates and so the `for_each` keying, the variable shape, the cloud-init and the
outputs are real and checkable before that decision is made. Replacing it is one
resource block plus a provider block.

## The one thing to understand before running anything here

The `for_each` key is `mm_node_id`, so **every key missing from
`desired_nodes.auto.tfvars.json` is a machine this module destroys.** That file is
written atomically by `TfvarsWriter` precisely because a truncated map is not a
parse error — it is a *shorter* map, and a shorter map is a teardown.

`TfvarsWriter` also refuses to write a file that removes more than half the
rented entries still meant to run unless the caller says it means to. Owned and
leased entries count neither way: `for_each` never sees them (`local.rented_nodes`
in `main.tf`), so removing one destroys nothing, and counting them would dilute
the guard for the rented ones. The runner passes
that only for `fleet=off`. A removal of a node that `DesiredStore::teardown` has
already acted on (`gone` or `destroying` in `mm_fleet_nodes`) is set aside rather
than judged — and leaves the baseline too — because refusing it would keep the
destroyed node in this file, and the next apply would create it again.

## Invariants enforced here as well as in mm-core

`variables.tf` validates flavor and ownership against the same sets as V034's
`CHECK` constraints, and re-asserts the deadline rule in both directions: a rented
node **must** carry `destroy_deadline`, and a non-rented node **must not**. mm-core
enforces this in the database and in `DesiredStore`; this is the third place, and
it is the one holding the provider API token.

`destroy_deadline` is deliberately absent from `triggers_replace`. Extending a
node's life must not destroy it.

## Running it

```bash
cd terraform/fleet
terraform init
terraform validate
terraform plan     # read the destroy list before every apply
```

**Read the destroy list.** This module's failure mode is not a broken apply; it is
a successful one that removes machines with viewers on them.
