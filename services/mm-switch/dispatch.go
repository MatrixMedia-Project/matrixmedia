// SPDX-License-Identifier: Apache-2.0
//
// Crash-contained subscriber dispatch (ADR-04 Phase 1 / A1).
//
// Every source fan-out loop used to call subscriber handlers bare, on
// plain goroutines — one panic anywhere in a handler (recorder muxing,
// viewer rewrite, relay forward) terminated the whole process: every
// stream, every viewer, every recording. safeDispatch converts such a
// panic into a contained failure: the offending subscriber is
// quarantined (unsubscribed) and, if it belongs to a recorder, the
// recording transitions to the "failed" state. Live fan-out continues.

package main

import (
	"log"
	"sync"

	"github.com/pion/rtp"
)

// onSubscriberQuarantined is invoked — outside any source lock — for
// every subscriber removed from a fan-out after panicking. The default
// implementation flips the matching recorder (if the subscriber was
// one) to RecordingFailed. Package-level var so tests can intercept.
var onSubscriberQuarantined = func(sourceID, subID string) {
	if mediaSwitch != nil {
		mediaSwitch.FailRecorderBySubscriber(subID, "panic in packet handler")
	}
}

// safeDispatch invokes a single subscriber handler with a recover()
// guard. Returns false if the handler panicked; the caller must then
// quarantine that subscriber after releasing the source's read lock
// (the locking contract forbids map mutation under RLock).
func safeDispatch(sourceID, subID string, h PacketHandler, kind string, pkt *rtp.Packet) (ok bool) {
	defer func() {
		if r := recover(); r != nil {
			ok = false
			recorderPanicsTotal.Inc()
			log.Printf("[fanout:%s] subscriber %s panicked (quarantining): %v",
				sourceID, subID, r)
		}
	}()
	h(kind, pkt)
	return true
}

// dispatchAll fans one packet out to every subscriber. The caller must
// hold the source's read lock. Returns the IDs of subscribers that
// panicked (nil in the common case — no allocation on the hot path).
func dispatchAll(sourceID string, subs map[string]PacketHandler, kind string, pkt *rtp.Packet) []string {
	var panicked []string
	for id, h := range subs {
		if !safeDispatch(sourceID, id, h, kind, pkt) {
			panicked = append(panicked, id)
		}
	}
	return panicked
}

// dispatchAllPackets fans a burst of packets (e.g. all RTP packets of
// one video frame) out to every subscriber. A subscriber that panics
// receives none of the remaining packets in the burst. Caller must
// hold the source's read lock.
func dispatchAllPackets(sourceID string, subs map[string]PacketHandler, kind string, pkts []*rtp.Packet) []string {
	var panicked []string
	for id, h := range subs {
		for _, pkt := range pkts {
			if !safeDispatch(sourceID, id, h, kind, pkt) {
				panicked = append(panicked, id)
				break
			}
		}
	}
	return panicked
}

// quarantineSubscribers removes panicked subscribers from a source's
// subscriber map (write lock), then runs the quarantine hook outside
// the lock — onSubscriberQuarantined may take other locks (recorder
// mutex, source mutex via unsubscribe) and must never nest inside.
func quarantineSubscribers(sourceID string, mu *sync.RWMutex, subs map[string]PacketHandler, ids []string) {
	if len(ids) == 0 {
		return
	}
	mu.Lock()
	for _, id := range ids {
		delete(subs, id)
	}
	mu.Unlock()
	for _, id := range ids {
		onSubscriberQuarantined(sourceID, id)
	}
}
