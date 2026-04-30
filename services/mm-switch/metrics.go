package main

import (
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"
)

// Auth-failure counter — incremented on every rejected request, tagged
// with the rejection reason so we can alert specifically on
// `invalid_signature` (real attack signal) vs `expired` (clock skew or
// stale clients) vs `missing_header` (probes).
//
// Used by Grafana to plot rejection rate / 5min, and by alertmanager
// to fire when sustained rejections cross a threshold (>1/sec for
// 10 min = real outage or attack, not a one-off bad client).
var authRejectionsTotal = promauto.NewCounterVec(
	prometheus.CounterOpts{
		Name: "mm_switch_auth_rejections_total",
		Help: "Number of HMAC auth-middleware rejections, by reason.",
	},
	[]string{"reason"},
)

// Reason buckets we increment from auth.go. Centralised here so every
// branch uses the same label values (Grafana queries depend on these).
const (
	rejReasonMissingHeader   = "missing_header"
	rejReasonWrongScheme     = "wrong_scheme"
	rejReasonMalformed       = "malformed"
	rejReasonInvalidPayload  = "invalid_payload"
	rejReasonInvalidSig      = "invalid_signature"
	rejReasonExpired         = "expired"
	rejReasonWrongRole       = "wrong_role"
)
