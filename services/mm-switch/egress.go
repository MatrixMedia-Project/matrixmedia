// SPDX-License-Identifier: Apache-2.0
//
// Per-stream egress byte counters (FR-302a/b, design §17.2).
//
// The prepaid wallet meters egress in GB, and until now nothing counted bytes at
// all — `metrics.go` has no byte accounting, so there were no usage events to rate
// and the wallet could be charged but nothing generated the charges.
//
// ── Why this is not a Prometheus metric ─────────────────────────────────────
//
// Billing data and operational metrics have incompatible requirements, and
// conflating them is a mistake that only shows up on the invoice:
//
//   - A Prometheus label carrying a stream id is an unbounded cardinality bomb —
//     every broadcast that ever ran would leave a permanent time series behind.
//     (The same reason `mm_broadcast_viewers` is labelled by tier, not stream.)
//   - Scrapes are SAMPLED and lossy by design. A lost sample is a lost operational
//     data point, which is fine, and unbilled revenue, which is not (§17.2).
//   - A counter reset reads as a huge negative delta, and Prometheus papers over
//     that with `rate()` heuristics that are wrong for money.
//
// So egress is exposed on an **authenticated API endpoint** that mm-core polls and
// turns into idempotent usage events. Prometheus still gets a total, unlabelled by
// stream, for dashboards.
//
// ── The reset problem, made visible instead of hidden ───────────────────────
//
// These counters live in memory and die with the node — which for an ephemeral
// fan-out node is not an edge case, it is the normal end of its life. Full
// durability needs the buffer-and-acknowledge protocol of §17.5, which is not built.
//
// What is built is the part that stops a reset being *silently* mis-billed: every
// response carries an `epoch` unique to this process and a `since` timestamp. mm-core
// may only subtract two readings that share an epoch. A changed epoch means "these
// counters restarted, the bytes between my last poll and that restart are gone" —
// which is lost revenue mm-core can at least COUNT, rather than a negative delta it
// silently treats as zero or as billions.

package main

import (
	"crypto/rand"
	"encoding/hex"
	"log"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Per-packet transport overhead not present in an RTP packet's own marshalled size.
//
// A provider bills what leaves the NIC. `rtp.Packet.MarshalSize()` counts the RTP
// header and payload only, so it understates real egress by the UDP header (8), the
// IPv4 header (20) and SRTP's authentication tag (10) — about 38 bytes per packet,
// which at ~1200-byte payloads is ~3%, and much more for small audio packets.
//
// Under-counting is the safe direction (we bill less than we pay, and eat the
// difference as margin) but it is a SYSTEMATIC error, so it is a configurable
// constant rather than a silent omission. **Calibrate it against the provider's own
// egress figure on the first node** — that is the measurement, not this default.
const defaultEgressOverheadBytes = 38

const egressOverheadEnv = "MM_SWITCH_EGRESS_OVERHEAD_BYTES"

var egressOverheadBytes = loadEgressOverheadBytes()

func loadEgressOverheadBytes() int64 {
	raw := os.Getenv(egressOverheadEnv)
	if raw == "" {
		return defaultEgressOverheadBytes
	}
	n, err := strconv.ParseInt(raw, 10, 64)
	if err != nil || n < 0 {
		// Not fatal, and not silent: a bad value must not make the meter lie, and
		// must not stop a node serving either.
		logEgressOverheadProblem(raw)
		return defaultEgressOverheadBytes
	}
	return n
}

// Package-level var so a test can intercept it instead of capturing stderr.
var logEgressOverheadProblem = func(raw string) {
	log.Printf("[egress] %s=%q is not a non-negative integer — using %d",
		egressOverheadEnv, raw, defaultEgressOverheadBytes)
}

// egressEpoch identifies this process's counters.
//
// mm-core may only subtract two readings that share it. Generated once at startup —
// a restart therefore changes it, which is exactly the signal that says "do not
// compute a delta across this".
var egressEpoch = newEgressEpoch()

// Stdlib only: a process identifier that cannot repeat. A pid can be reused within
// a second, and reusing an epoch is exactly the failure this value exists to prevent
// — mm-core would subtract across a restart and bill the difference as usage.
func newEgressEpoch() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		// Unreachable in practice, and a time-based fallback is still unique enough
		// to keep two readings from the same process comparable — which is all the
		// epoch is for.
		return "t" + strconv.FormatInt(time.Now().UnixNano(), 36)
	}
	return hex.EncodeToString(b[:])
}

// egressStartedAt is when these counters began. Paired with the epoch so a reader
// can see how much history a reading actually covers.
var egressStartedAt = time.Now().UTC()

// closedEgress accumulates bytes from viewers that have gone away.
//
// Without this, every byte delivered to a viewer would vanish from the total when
// that viewer disconnected — and a viewer disconnecting is the normal case, not the
// exception. Keyed by BILLING source, so it is bounded by the number of broadcasts
// this process has served rather than by the number of viewers.
var closedEgress = struct {
	sync.Mutex
	bytes map[string]int64
}{bytes: make(map[string]int64)}

// recordClosedViewerEgress folds a departing viewer's counter into the running
// total. Called from viewer teardown, never on the packet path.
func recordClosedViewerEgress(billingSource string, bytes int64) {
	if billingSource == "" || bytes <= 0 {
		return
	}
	closedEgress.Lock()
	closedEgress.bytes[billingSource] += bytes
	closedEgress.Unlock()
}

// EgressReading is one source's cumulative egress within one epoch.
type EgressReading struct {
	Source string `json:"source"`
	Bytes  int64  `json:"bytes"`
}

// EgressReport is what mm-core polls.
type EgressReport struct {
	// Epoch of these counters. **Two readings with different epochs must not be
	// subtracted**: the process restarted and the bytes in between are gone.
	Epoch string `json:"epoch"`
	// When this epoch's counters began.
	Since time.Time `json:"since"`
	// Per-packet overhead included in the byte figures, so a reader can tell what
	// was counted and recompute if it is recalibrated.
	OverheadBytesPerPacket int64           `json:"overhead_bytes_per_packet"`
	Sources                []EgressReading `json:"sources"`
}

// egressSnapshot merges the closed-viewer totals with the live viewers' counters.
//
// O(viewers), on the read path only. The packet path does one atomic add and no map
// lookup and no lock, which is the reason the counters are per-viewer rather than a
// shared map keyed by source.
func egressSnapshot(viewers []*Viewer) EgressReport {
	totals := make(map[string]int64)

	closedEgress.Lock()
	for src, n := range closedEgress.bytes {
		totals[src] = n
	}
	closedEgress.Unlock()

	for _, v := range viewers {
		src, bytes := v.EgressSnapshot()
		if src == "" || bytes <= 0 {
			continue
		}
		totals[src] += bytes
	}

	out := EgressReport{
		Epoch:                  egressEpoch,
		Since:                  egressStartedAt,
		OverheadBytesPerPacket: egressOverheadBytes,
		Sources:                make([]EgressReading, 0, len(totals)),
	}
	for src, n := range totals {
		out.Sources = append(out.Sources, EgressReading{Source: src, Bytes: n})
	}
	return out
}

// isProgrammeSourceID reports whether a source id names a broadcast.
//
// mm-core registers a stream's programme as `stream-{broadcast_id}` (client.rs) and
// an ad creative as `ad-{user}-{timestamp}` (ads.rs). Only the first names a
// broadcast, which is why only the first may move a viewer's billing attribution.
//
// ⚠️ This is a STRING CONTRACT with mm-core. If the programme source id shape ever
// changes on that side without changing here, every viewer's `billingSource` stays
// empty, every byte is attributed to nothing, and egress silently stops being
// metered. `the_programme_source_prefix_matches_mm_core` pins it.
func isProgrammeSourceID(id string) bool {
	return strings.HasPrefix(id, programmeSourcePrefix)
}

// The prefix mm-core uses. Kept as a named constant so the test that pins it has
// something to point at.
const programmeSourcePrefix = "stream-"
