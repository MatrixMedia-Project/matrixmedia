# MatrixMedia Operations Runbook

## Incident Response

### Service Down

**Symptom**: `mm_up` = 0, health endpoint returns 5xx or unreachable

**Diagnosis**:
1. Check process: `systemctl status matrixmedia` or `docker ps | grep mm-core`
2. Check logs: `journalctl -u matrixmedia -n 100` or `docker logs mm-core --tail=100`
3. Check resources: disk (DB), memory, CPU

**Resolution**:
- **Process crashed**: Check logs for panic/OOM. Restart service.
- **Database locked**: Check for long-running queries. Restart may help for SQLite.
- **Port conflict**: `lsof -i :6167` to find conflicting process.

### SFU Unreachable

**Symptom**: `mm_sfu_health_status` = 0, circuit state = open, streams fail to start

**Diagnosis**:
1. Test LiveKit: `curl http://livekit:7880` (should return HTML)
2. Check LiveKit logs: `docker logs livekit --tail=100`
3. Verify TURN is working: coturn logs

**Resolution**:
- **LiveKit down**: Restart LiveKit container.
- **Network partition**: Check container network connectivity.
- **Credentials wrong**: Verify `MM_SFU_LIVEKIT_API_KEY/SECRET` match LiveKit config.

### High Error Rate

**Symptom**: `mm_http_requests_total{status=~"5.."}` rate > 5%

**Diagnosis**:
1. Check which endpoint: query metrics by path label
2. Check logs for stack traces
3. Check dependencies: DB, homeserver, SFU

**Resolution**:
- **DB errors**: Check disk space, connection pool, locks
- **Homeserver unreachable**: Check network, verify as_token valid
- **SFU errors**: See above

### Stream Join Failures

**Symptom**: `mm_http_requests_total{path=~".*join.*",status="409"}` elevated

**Diagnosis**:
- 409 MM_ROOM_FULL: room at participant cap
- 409 MM_STREAM_ACTIVE: duplicate create attempt
- 404 MM_STREAM_NOT_FOUND: stale state event

**Resolution**:
- Verify capacity limits in config
- Check for stuck active streams: `GET /admin/streams`
- Force-stop stuck streams via admin API

## Routine Operations

### Graceful Restart

```bash
# Systemd
systemctl reload matrixmedia   # Reloads config without restart
systemctl restart matrixmedia  # Full restart with graceful drain

# Docker
docker-compose restart mm-core
```

mm-core waits for active WebSocket connections to drain (configurable, default 30s).

### Rolling Upgrade (Kubernetes)

```bash
# 1. Update image tag
helm upgrade matrixmedia ./infra/helm/matrixmedia \
  --set image.tag=0.2.0 \
  --reuse-values

# 2. Watch rollout
kubectl rollout status deployment/matrixmedia

# 3. Rollback if needed
helm rollback matrixmedia
```

### Config Reload

Environment variable changes require restart. TOML file changes: send SIGHUP (future feature).

### Scaling

**Vertical**: increase CPU/memory limits in deployment.
**Horizontal**: increase replicas in Helm values. Requires PostgreSQL (SQLite does not support multiple writers).

## Maintenance

### Database Backup (PostgreSQL)

```bash
# Backup
pg_dump matrixmedia > backup-$(date +%Y%m%d).sql

# Restore
psql matrixmedia < backup-20260404.sql
```

### Database Backup (SQLite)

```bash
# Online backup (safe while running)
sqlite3 /var/lib/matrixmedia/mm.db ".backup /backup/mm-$(date +%Y%m%d).db"

# Restore (service must be stopped)
systemctl stop matrixmedia
cp /backup/mm-20260404.db /var/lib/matrixmedia/mm.db
systemctl start matrixmedia
```

### Migration: SQLite -> PostgreSQL

1. Export SQLite: `sqlite3 mm.db ".dump" > mm-dump.sql`
2. Clean up syntax differences (sed script required)
3. Import to Postgres
4. Update `MM_DATABASE_URL` to Postgres connection string
5. Restart

### Log Rotation

With systemd: use `systemd-journald` (built-in rotation).
With docker: use `json-file` driver with size limits:
```yaml
logging:
  driver: json-file
  options:
    max-size: "100m"
    max-file: "10"
```

### Retention Cleanup

```bash
# Trigger recording cleanup
curl -X POST http://admin:6168/_mm/admin/v1/recordings/cleanup \
  -H "Authorization: Bearer $ADMIN_TOKEN"
```

## Monitoring

### Key Metrics to Watch

- `mm_streams_active` -- should be > 0 during business hours
- `mm_sfu_health_status` -- must be 1
- `mm_http_requests_total` rate -- baseline traffic
- `mm_join_latency_seconds` p95 -- alert if > 2s

See `docs/slo-sli.md` for full SLO definitions.

### Dashboard Access

Grafana: import `infra/grafana/matrixmedia-overview.json`
Prometheus alerts: `infra/prometheus/matrixmedia-alerts.yml`
