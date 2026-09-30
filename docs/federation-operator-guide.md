# Federation Operator Guide

Short guide for MM operators enabling federation.

## Where federation is configured

`federation.enabled`, `federation.allow_list` and `federation.deny_list` are settings in
Operator Console → System → **Settings** → **Federation**. They apply when you press
**Save**, with no restart (so does `federation.validation_timeout_secs`;
`federation.validation_cache_ttl_secs` needs **Apply & restart**).

`MM_FEDERATION_ENABLED`, `MM_FEDERATION_ALLOW_LIST`, `MM_FEDERATION_DENY_LIST` and the
`[federation]` TOML table only seed mm-core's first start. After that the dashboard
value wins: a changed env var or TOML value is ignored (mm-core logs a warning naming the
env vars it ignores, and the Settings page marks them). They are used again only as the
break-glass fallback: with `MM_SETTINGS_SAFE_MODE=1`, mm-core ignores every dashboard
value and runs from file + env. See [deploy/docs/settings.md](../deploy/docs/settings.md).

## Enabling Federation

1. In Settings → Federation, turn on `federation.enabled`
2. Decide on trust model:
   - **Open federation** (empty allow_list): allow all servers not in deny_list
   - **Closed federation** (populated allow_list): only listed servers can federate
3. Press **Save** (no restart)

## Common Deployment Patterns

The snippets show each pattern as the `[federation]` TOML seed for a new deployment; on a
running one, set the same values in Settings → Federation.

### Pattern 1: Open Federation
```toml
[federation]
enabled = true
allow_list = []
deny_list = []  # Add spam servers as needed
```
Use case: public MM deployment accepting federated users from any Matrix server.

### Pattern 2: Community Whitelist
```toml
[federation]
enabled = true
allow_list = ["matrix.org", "element.io", "mozilla.org"]
deny_list = []
```
Use case: MM deployment for a specific community network.

### Pattern 3: Single-Server (No Federation)
```toml
[federation]
enabled = false
```
Use case: private deployment, users only from one Matrix server.

## Adding/Removing Servers

1. In Settings → Federation, edit `federation.allow_list` or `federation.deny_list`
2. Press **Save**. The new list applies to the next join, with no restart.

Editing the env var or the TOML file instead changes nothing once mm-core has started
(see [Where federation is configured](#where-federation-is-configured)).

## Monitoring Federated Traffic

Dashboard queries:
```promql
# Federation success rate
rate(mm_federation_validations_total[5m]) / 
  (rate(mm_federation_validations_total[5m]) + rate(mm_federation_rejections_total[5m]))

# Top federated servers (requires labeling)
sum by (server_name) (rate(mm_federated_joins_total[1h]))
```

Alert recommendations:
- `mm_federation_validation_errors_total` rate > 5/min → foreign server issues
- `mm_federation_rejections_total` rate > 20/min → potential abuse

## Troubleshooting

### Federated user can't join

1. Check `mm_federation_rejections_total` metric
2. Check logs for federation decision: `federation_check server=... decision=...`
3. Verify foreign server is reachable: `curl https://<server>:8448/_matrix/federation/v1/version`
4. Verify token is fresh (OpenID tokens expire in 1 hour typically)

### High validation error rate

1. Check foreign server's `/_matrix/federation/v1/openid/userinfo` endpoint
2. Foreign server may be down, slow, or non-federation-compliant
3. Consider adding to deny_list temporarily

### Allow/deny list not applied

1. Check the lists in Settings → Federation: that is what runs. An env var or TOML
   value changed after the first start is ignored.
2. Check that `federation.enabled` is on there
3. Check that mm-core is not in safe mode (a banner on the Settings page): in safe mode
   it runs from file + env, and dashboard values apply only once safe mode ends
4. Server names are case-insensitive but list entries are case-sensitive

## Security Best Practices

- **Default-deny for sensitive deployments**: use allow_list mode
- **Monitor rejection rates**: sudden spikes indicate abuse
- **Keep validation_cache_ttl short** (default 300s) in high-churn environments
- **Use deny_list** for known-bad servers even with allow_list (defense in depth)
- **Combine with E2EE**: even federated users can't eavesdrop on E2EE streams
