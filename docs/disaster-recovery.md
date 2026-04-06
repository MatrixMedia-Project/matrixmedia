# Disaster Recovery

## Recovery Time Objectives (RTO)

| Scenario | RTO | RPO |
|---|---|---|
| mm-core pod crash | 30s (k8s auto-restart) | 0 |
| SFU failure | 2 min (manual intervention) | Live streams lost |
| Database failure | 15 min (restore from backup) | 24h (daily backup) |
| Datacenter failure | 1h (manual failover) | 24h |

## Backup Strategy

### Automated Backups

PostgreSQL with pg_dump nightly to S3:
```bash
# crontab
0 2 * * * pg_dump matrixmedia | gzip | aws s3 cp - s3://backups/mm/mm-$(date +\%Y\%m\%d).sql.gz
```

### What to Back Up

| Component | Strategy | Retention |
|---|---|---|
| PostgreSQL database | pg_dump nightly | 30 days |
| S3 media storage | Cross-region replication | forever |
| Config files | Git repo | forever |
| Secrets | Vault/KMS | forever |

### What Survives a Database Loss

- S3 recordings remain accessible (URLs are in DB, but files persist)
- Matrix state events persist on homeserver
- Active streams lost (no persistent connection state)

## Recovery Procedures

### Restore Database from Backup

```bash
# 1. Stop service
systemctl stop matrixmedia

# 2. Download latest backup
aws s3 cp s3://backups/mm/mm-latest.sql.gz .

# 3. Restore
gunzip -c mm-latest.sql.gz | psql matrixmedia

# 4. Start service
systemctl start matrixmedia

# 5. Verify
curl http://localhost:6168/_mm/admin/v1/health
```

### Rebuild from Matrix State

If database is lost but Matrix state is intact:
- `mm_rooms` can be rebuilt from active `com.matrixmedia.stream` state events
- Recordings in S3 can be re-registered if mxc_url in state event is known
- Historical stream data is lost

## Testing

Quarterly DR drill:
1. Spin up staging environment
2. Restore from production backup
3. Verify service comes up
4. Run E2E test suite
5. Document time-to-recovery
