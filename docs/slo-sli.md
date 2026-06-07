# MatrixMedia SLOs and SLIs

## Service Level Objectives

| SLO | Target | SLI (Metric) |
|---|---|---|
| Stream creation success rate | 99.9% | `1 - (rate(mm_http_requests_total{path="/streams",status=~"5.."}[5m]) / rate(mm_http_requests_total{path="/streams"}[5m]))` |
| Join-to-audio latency p95 | < 2s | `histogram_quantile(0.95, rate(mm_join_latency_seconds_bucket[5m]))` |
| SFU availability | 99.5% | `avg_over_time(mm_sfu_health_status[1h])` |
| API 5xx error rate | < 0.1% | `rate(mm_http_requests_total{status=~"5.."}[5m]) / rate(mm_http_requests_total[5m])` |
| Auth validation success | 99.95% | `1 - (rate(mm_auth_failures_total[5m]) / rate(mm_auth_validations_total[5m]))` |

## Error Budget

For 99.9% availability over 30 days: ~43 minutes of downtime permitted per month.

## Alerting Strategy

- **Critical alerts** (SLO breach): page on-call
- **Warning alerts** (trending toward breach): notify team channel
- **Info** (unusual patterns): log only

## SLI Collection

All SLIs are emitted as Prometheus metrics from mm-core on port 9090.
Scrape config example for Prometheus:

```yaml
scrape_configs:
  - job_name: mm-core
    static_configs:
      - targets: ['mm-core:9090']
    scrape_interval: 15s
  - job_name: mm-switch
    static_configs:
      - targets: ['mm-switch:7890']
    scrape_interval: 15s
```

> Alert rules select these with `up{job=~"mm-core|mm-switch"}` — keep the job
> names here in sync with `infra/prometheus/matrixmedia-alerts.yml`.
