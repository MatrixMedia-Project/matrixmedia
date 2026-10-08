// SPDX-License-Identifier: Apache-2.0

package main

import (
	"fmt"
	"testing"
)

func acceptAll(w *seqWindow, seqs ...uint16) []uint16 {
	var got []uint16
	for _, s := range seqs {
		if w.accept(s) {
			got = append(got, s)
		}
	}
	return got
}

func TestSeqWindowDropsDuplicatesAndKeepsLateRepairs(t *testing.T) {
	var w seqWindow
	got := acceptAll(&w, 10, 11, 13, 11, 12, 12, 13, 14, 10, 14)
	if want := []uint16{10, 11, 13, 12, 14}; fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("accepted %v, want %v", got, want)
	}
}

func TestSeqWindowAcrossTheWrap(t *testing.T) {
	var w seqWindow
	got := acceptAll(&w, 65533, 65535, 0, 65534, 1, 65535, 0, 2)
	if want := []uint16{65533, 65535, 0, 65534, 1, 2}; fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("accepted %v, want %v", got, want)
	}
}

// A slot reused after the window moved on must not remember the old packet: seq 5 and
// seq 5+seqWindowSize share a bit.
func TestSeqWindowForgetsSlotsItHasMovedPast(t *testing.T) {
	var w seqWindow
	acceptAll(&w, 5, 6)
	if !w.accept(5 + seqWindowSize - 1) {
		t.Fatal("rejected a new packet within one window of the last")
	}
	if !w.accept(5 + seqWindowSize) {
		t.Fatal("rejected seq 5+window: its slot still remembered seq 5")
	}
	// A big jump forward clears everything.
	if !w.accept(5 + 3*seqWindowSize) {
		t.Fatal("rejected a jump forward")
	}
	if !w.accept(5 + 3*seqWindowSize - 1) {
		t.Fatal("rejected a late packet just behind the jump")
	}
}

func TestSeqWindowDropsPacketsOlderThanTheWindow(t *testing.T) {
	var w seqWindow
	acceptAll(&w, 5000)
	if w.accept(5000 - seqWindowSize) {
		t.Fatal("accepted a packet a full window behind the newest")
	}
}

// Scattered stale packets (probe re-sends of old history) never re-anchor the window; a
// long consecutive run far away (a restarted sequence) does.
func TestSeqWindowResyncsOnlyOnAConsecutiveRun(t *testing.T) {
	var w seqWindow
	acceptAll(&w, 30000)
	for i := 0; i < 3*seqResyncRun; i++ {
		if w.accept(uint16(20000 + 7*i)) {
			t.Fatalf("scattered stale packet %d re-anchored the window", 20000+7*i)
		}
	}
	accepted := 0
	for i := 0; i < seqResyncRun+5; i++ {
		if w.accept(uint16(100 + i)) {
			accepted++
		}
	}
	if accepted != 6 {
		t.Fatalf("after a restarted sequence, accepted %d packets, want the run's last 6", accepted)
	}
	if w.accept(uint16(100 + seqResyncRun + 4)) {
		t.Fatal("after re-anchoring, a duplicate in the new sequence got through")
	}
}
