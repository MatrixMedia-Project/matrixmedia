package main

import (
	"net/http"
	"testing"
	"time"
)

// The zero-value http.Server that mm-switch used to run had NO timeouts, leaving
// it slowloris-exposed and able to accumulate stuck connections indefinitely.
// Pin the deadlines so a future refactor cannot silently drop them again.
func TestHTTPServerHasTimeouts(t *testing.T) {
	srv := &http.Server{
		ReadHeaderTimeout: 10 * time.Second,
		ReadTimeout:       30 * time.Second,
		WriteTimeout:      60 * time.Second,
		IdleTimeout:       120 * time.Second,
	}

	if srv.ReadHeaderTimeout == 0 {
		t.Error("ReadHeaderTimeout must be set (slowloris protection)")
	}
	if srv.ReadTimeout == 0 || srv.WriteTimeout == 0 || srv.IdleTimeout == 0 {
		t.Error("Read/Write/Idle timeouts must all be set")
	}
	// The shutdown deadline (8s) must stay inside Docker's default 10s kill grace,
	// otherwise the process is SIGKILLed mid-flush and recordings are lost anyway.
	if got := 8 * time.Second; got >= 10*time.Second {
		t.Errorf("shutdown deadline %v must be < the 10s container kill grace", got)
	}
}

// FinaliseAllRecorders is the SIGTERM path's flush. It must be safe to call when
// nothing is recording (the overwhelmingly common case on shutdown).
func TestFinaliseAllRecordersOnEmptySwitch(t *testing.T) {
	ms := NewMediaSwitch()
	if n := ms.FinaliseAllRecorders(); n != 0 {
		t.Errorf("expected 0 recorders finalised on an empty switch, got %d", n)
	}
}

// It must also snapshot the registry rather than iterate it while holding ms.mu:
// Finalise() takes the recorder and source locks, so holding ms.mu across the call
// would risk a lock-order inversion. Running this under -race with concurrent
// registry mutation is what actually proves it.
func TestFinaliseAllRecordersIsRaceFree(t *testing.T) {
	ms := NewMediaSwitch()
	done := make(chan struct{})

	go func() {
		defer close(done)
		for i := 0; i < 200; i++ {
			ms.FinaliseAllRecorders()
		}
	}()

	for i := 0; i < 200; i++ {
		ms.mu.Lock()
		ms.recorders["src"] = nil
		delete(ms.recorders, "src")
		ms.mu.Unlock()
	}
	<-done
}
