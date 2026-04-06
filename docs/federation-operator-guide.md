# Federation Operator Guide

Short guide for MM operators enabling federation.

## Enabling Federation

1. Set `MM_FEDERATION_ENABLED=true` (or `federation.enabled = true` in TOML)
2. Decide on trust model:
   - **Open federation** (empty allow_list): allow all servers not in deny_list
   - **Closed federation** (populated allow_list): only listed servers can federate
3. Restart MM server

## Common Deployment Patterns

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

Allow/deny lists are static per deployment. To add a server:

1. Edit config file or env var
2. Restart MM server (config reload without restart is future work)

Runtime updates via admin API are on the roadmap.

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

1. Restart required after config change
2. Check that `MM_FEDERATION_ENABLED=true`
3. Server names are case-insensitive but list entries are case-sensitive

## Security Best Practices

- **Default-deny for sensitive deployments**: use allow_list mode
- **Monitor rejection rates**: sudden spikes indicate abuse
- **Keep validation_cache_ttl short** (default 300s) in high-churn environments
- **Use deny_list** for known-bad servers even with allow_list (defense in depth)
- **Combine with E2EE**: even federated users can't eavesdrop on E2EE streams
