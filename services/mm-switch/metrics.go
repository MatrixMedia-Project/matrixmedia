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

// ---------------------------------------------------------------------------
// Recorder crash-containment metrics (ADR-04 Phase 1).
// ---------------------------------------------------------------------------

// Panics recovered in the fan-out subscriber dispatch (dispatch.go) or
// in a recorder's async writer goroutine (record_webm.go). Any non-zero
// rate is an incident signal: before Phase 1 each of these was a whole-
// process crash taking down every stream, viewer and recording.
var recorderPanicsTotal = promauto.NewCounter(
	prometheus.CounterOpts{
		Name: "mm_switch_recorder_panics_total",
		Help: "Recovered panics in fan-out subscriber dispatch or recorder writer goroutines.",
	},
)

// WebM block-write failures (e.g. ENOSPC, stalled volume). In async
// mode a streak of these flips the recording to the failed state; in
// legacy inline mode they are logged only (pre-Phase-1 behavior).
var recorderWriteErrorsTotal = promauto.NewCounter(
	prometheus.CounterOpts{
		Name: "mm_switch_recorder_write_errors_total",
		Help: "WebM block write failures across all recorders.",
	},
)

// Packets dropped because a recorder's bounded write queue was full —
// backpressure shed to keep disk stalls out of the live fan-out path.
// Sustained growth means the recording volume cannot keep up.
var recorderDroppedPacketsTotal = promauto.NewCounter(
	prometheus.CounterOpts{
		Name: "mm_switch_recorder_dropped_packets_total",
		Help: "RTP packets dropped due to a full recorder write queue (backpressure).",
	},
)

// Packets shed by a viewer's bounded write queue in async mode
// (MM_SWITCH_ASYNC_VIEWERS). Dropping is by design — a viewer too slow to keep up must
// not stall the fan-out for everyone else — but a silent drop is indistinguishable from
// a healthy stream. Without this series nobody can tell whether flipping the flag helped
// or is quietly degrading a viewer.
var viewerDroppedPacketsTotal = promauto.NewCounter(
	prometheus.CounterOpts{
		Name: "mm_switch_viewer_dropped_packets_total",
		Help: "RTP packets dropped due to a full viewer write queue (async delivery only).",
	},
)

// Instantaneous depth of each recorder's bounded async write queue.
// Healthy steady state hovers near zero; approaching the queue
// capacity is the early-warning twin of the dropped-packets counter.
var recorderQueueDepth = promauto.NewGaugeVec(
	prometheus.GaugeOpts{
		Name: "mm_switch_recorder_queue_depth",
		Help: "Current depth of a recorder's bounded async write queue.",
	},
	[]string{"recording_id"},
)
