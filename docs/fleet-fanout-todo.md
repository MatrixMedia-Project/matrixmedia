# Fleet fan-out TODO (from the 2026-11-13 threshold proof)

Status: threshold detection, ordering, Hetzner rent, anonymous proxy
admission, and teardown are proven on stage (PRs #192, #193). The one
open item is the feeder never becoming healthy.

## TODO-1 (unblock): bootstrap API-rented fan-out nodes

Problem: an API-rented feeder boots stock Ubuntu with a cloud-init that
only writes `/etc/mm-switch.env` (node id + flavor, deliberately no
secret), so nothing installs or can start mm-switch. Observed 2026-11-13:
Hetzner box `2.31.213.100` (node
`bc-348373e4-3a99-4d57-8b9a-6d1fa2f02f92-fanout-0`) pinged for 45+ min
with nothing listening and no boot report; teardown afterwards worked
(`destroyed:1`, machine deleted, provider re-checked `ok`).

Do: extend the fan-out cloud-init (`fanout_cloud_init`) to install
Docker, run the pinned mm-switch image in fan-out flavor, and post the
boot report. Carry `MM_SWITCH_AUTH_SECRET` in user-data — the terraform
path already does this, so the trust boundary is unchanged.

Accept: a rented feeder reaches `healthy`, registers, and serves
overflow viewers; re-run the two-wave proof (85 + 40 bots) and show
origin egress flat.

## TODO-2 (hardening): per-node feeder secrets

A leaked global secret compromises every stream; a leaked per-node
secret compromises one box. Mint a unique secret per rented node at
rent time and make token minting node-aware (join, proxy, cascade all
mint centrally today). Do this as its own design, not inside TODO-1.
