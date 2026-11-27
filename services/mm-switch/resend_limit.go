// SPDX-License-Identifier: Apache-2.0
//
// Resend limit — a bound on how much a viewer's loss reports can make us send.
//
// pion's NACK responder answers every request in full: each sequence number in each
// NACK is written to the network again, on a new goroutine, every time it is asked
// for. A receiver on a lossy link asks again every 100 ms (pion) or once per round
// trip (libwebrtc), so the resends grow with the loss, land on the same link, and
// cause more loss. Measured on 2026-11-26: a link 10% too small turned a 55 Mbps
// programme into 869 Mbps on the wire while viewers received 12 Mbps; on stage,
// 138 Mbps became about 1 Gbps for about 1 Mbps of requests. A client does not need
// a bad link to do this. It only needs to ask.
//
// The limit sits in front of the responder and removes requests before it sees them:
//
//   - one packet is sent again at most resendMaxPerPacket times, and not twice within
//     resendMinSpacing (LiveKit's rule: pkg/sfu/sequencer.go, maxAck = 3);
//   - resends per stream stay within a share of the programme's own packet rate, with
//     room for a burst (libwebrtc bounds retransmission bitrate the same way).
//
// The first rule stops a repeated request multiplying. The second is what holds when
// every packet is asked for once, which the first rule cannot see.

package main

import (
	"log"
	"math"
	"os"
	"strconv"
	"sync"
	"time"

	"github.com/pion/interceptor"
	"github.com/pion/rtcp"
	"github.com/pion/rtp"
)

const (
	resendMaxPerPacket = 3
	resendMinSpacing   = 100 * time.Millisecond

	// pion's responder keeps this many packets per stream (nack.ResponderSize default).
	// A request for anything older cannot be served, so it is not let through.
	resendHistory = 1024

	// Until one full window has shown the programme's packet rate, a stream gets a
	// fixed allowance. It is enough to repair a lost keyframe at join.
	resendFloorRate  = 25.0 // resends per second
	resendFloorBurst = 64.0

	resendBurstSeconds = 0.5 // the allowance can save up half a second of programme
	resendRateWindow   = time.Second

	defaultResendShare = 0.25
)

type resendSlot struct {
	seq   uint16
	used  bool
	count uint8
	last  time.Time
}

// resendLimiter decides, for one outgoing stream, whether a packet may be sent again.
type resendLimiter struct {
	mu    sync.Mutex
	now   func() time.Time
	share float64

	slots   [resendHistory]resendSlot
	highest uint16
	started bool

	windowStart time.Time
	windowCount int
	rate        float64 // programme packets per second; 0 until the first window closes

	tokens   float64
	refilled time.Time
}

func newResendLimiter(share float64, now func() time.Time) *resendLimiter {
	return &resendLimiter{now: now, share: share}
}

// noteSent records a packet on its way to the network. Only packets that advance the
// sequence are programme; a resend (same number again) must not count as programme,
// or resends would raise their own allowance.
func (l *resendLimiter) noteSent(seq uint16) {
	l.mu.Lock()
	defer l.mu.Unlock()
	now := l.now()
	if !l.started {
		l.started = true
		l.windowStart, l.refilled, l.tokens = now, now, resendFloorBurst
	} else if int16(seq-l.highest) <= 0 {
		return
	}
	l.highest = seq
	l.slots[seq%resendHistory] = resendSlot{seq: seq, used: true}

	l.windowCount++
	if elapsed := now.Sub(l.windowStart); elapsed >= resendRateWindow {
		l.refill(now) // settle the allowance at the old rate before the rate changes
		l.rate = float64(l.windowCount) / elapsed.Seconds()
		l.windowStart, l.windowCount = now, 0
	}
}

// allow reports whether seq may be sent again now, and counts it if so.
func (l *resendLimiter) allow(seq uint16) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	slot := &l.slots[seq%resendHistory]
	if !slot.used || slot.seq != seq {
		return false // never sent, or too old to still be in the responder's buffer
	}
	if slot.count >= resendMaxPerPacket {
		return false
	}
	now := l.now()
	if slot.count > 0 && now.Sub(slot.last) < resendMinSpacing {
		return false
	}
	l.refill(now)
	if l.tokens < 1 {
		return false
	}
	l.tokens--
	slot.count++
	slot.last = now
	return true
}

func (l *resendLimiter) refill(now time.Time) {
	rate, burst := resendFloorRate, resendFloorBurst
	if l.rate > 0 {
		rate = math.Max(resendFloorRate, l.share*l.rate)
		burst = math.Max(resendFloorBurst, resendBurstSeconds*l.rate)
	}
	l.tokens = math.Min(burst, l.tokens+now.Sub(l.refilled).Seconds()*rate)
	l.refilled = now
}

// resendLimitInterceptor applies a resendLimiter to every stream one PeerConnection
// sends. It must be registered BEFORE pion's NACK responder: interceptors read RTCP
// innermost first, so only an earlier one can take a request away from a later one.
type resendLimitInterceptor struct {
	interceptor.NoOp
	share float64

	mu      sync.Mutex
	streams map[uint32]*resendLimiter
}

type resendLimitFactory struct{ share float64 }

func (f resendLimitFactory) NewInterceptor(string) (interceptor.Interceptor, error) {
	return &resendLimitInterceptor{share: f.share, streams: map[uint32]*resendLimiter{}}, nil
}

func (r *resendLimitInterceptor) BindLocalStream(info *interceptor.StreamInfo, writer interceptor.RTPWriter) interceptor.RTPWriter {
	limiter := newResendLimiter(r.share, time.Now)
	ssrc := info.SSRC
	r.mu.Lock()
	r.streams[ssrc] = limiter
	r.mu.Unlock()

	return interceptor.RTPWriterFunc(func(header *rtp.Header, payload []byte, attributes interceptor.Attributes) (int, error) {
		// Resends negotiated as RTX (RFC 4588) leave on their own SSRC; they are not programme.
		if header.SSRC == ssrc {
			limiter.noteSent(header.SequenceNumber)
		}
		return writer.Write(header, payload, attributes)
	})
}

func (r *resendLimitInterceptor) UnbindLocalStream(info *interceptor.StreamInfo) {
	r.mu.Lock()
	delete(r.streams, info.SSRC)
	r.mu.Unlock()
}

func (r *resendLimitInterceptor) BindRTCPReader(reader interceptor.RTCPReader) interceptor.RTCPReader {
	return interceptor.RTCPReaderFunc(func(b []byte, a interceptor.Attributes) (int, interceptor.Attributes, error) {
		n, attr, err := reader.Read(b, a)
		if err != nil {
			return n, attr, err
		}
		if attr == nil {
			attr = make(interceptor.Attributes)
		}
		// The parsed packets are cached on the attributes and the responder reads the
		// same cache, so trimming them here is what the responder then acts on.
		pkts, err := attr.GetRTCPPackets(b[:n])
		if err != nil {
			return n, attr, nil //nolint:nilerr // malformed RTCP is the next reader's to reject
		}
		for _, pkt := range pkts {
			if nack, ok := pkt.(*rtcp.TransportLayerNack); ok {
				r.trim(nack)
			}
		}
		return n, attr, nil
	})
}

// trim removes from one NACK every sequence number that may not be sent again now.
func (r *resendLimitInterceptor) trim(nack *rtcp.TransportLayerNack) {
	r.mu.Lock()
	limiter := r.streams[nack.MediaSSRC]
	r.mu.Unlock()
	if limiter == nil {
		return // not a stream this connection sends; the responder ignores it too
	}
	keep := make([]uint16, 0, len(nack.Nacks))
	asked := 0
	for i := range nack.Nacks {
		nack.Nacks[i].Range(func(seq uint16) bool {
			asked++
			if limiter.allow(seq) {
				keep = append(keep, seq)
			}
			return true
		})
	}
	nack.Nacks = rtcp.NackPairsFromSequenceNumbers(keep)
	resendRequestsTotal.WithLabelValues("allowed").Add(float64(len(keep)))
	resendRequestsTotal.WithLabelValues("limited").Add(float64(asked - len(keep)))
}

// resendShare is the share of a stream's programme packet rate that may be sent again.
var resendShare = loadResendShare()

// loadResendShare reads MM_SWITCH_RESEND_BUDGET_PERCENT: a whole number from 0 to 100,
// default 25. At 0 a stream still gets the fixed floor allowance, so a lost keyframe at
// join can be repaired. Anything else keeps the default and says so: a typo in a safety
// limit must neither stop the switch nor switch the limit off.
func loadResendShare() float64 {
	raw := os.Getenv("MM_SWITCH_RESEND_BUDGET_PERCENT")
	if raw == "" {
		return defaultResendShare
	}
	percent, err := strconv.Atoi(raw)
	if err != nil || percent < 0 || percent > 100 {
		log.Printf("MM_SWITCH_RESEND_BUDGET_PERCENT=%q invalid (want 0-100), using default %d",
			raw, int(defaultResendShare*100))
		return defaultResendShare
	}
	return float64(percent) / 100
}
