package main

import (
	"net/http"
	"testing"
	"time"
)

// The zero-value http.Server that mm-switch used to run had NO timeouts, leaving it
// slowloris-exposed and able to accumulate stuck connections indefinitely.
//
// This asserts on newHTTPServer — the server main() ACTUALLY runs. The previous version of
// this test built its own http.Server literal and checked that, which proved nothing: it
// would have passed unchanged after someone deleted every timeout from main().
func TestHTTPServerHasTimeouts(t *testing.T) {
	srv := newHTTPServer(":0", http.NewServeMux())

	if srv.ReadHeaderTimeout == 0 {
		t.Error("ReadHeaderTimeout must be set (slowloris protection)")
	}
	if srv.ReadTimeout == 0 || srv.WriteTimeout == 0 || srv.IdleTimeout == 0 {
		t.Error("Read/Write/Idle timeouts must all be set")
	}
}

// The shutdown deadline must stay inside Docker's default 10s kill grace, or the process
// is SIGKILLed mid-flush and in-progress recordings are lost — exactly what the graceful
// path exists to prevent. Reads the real constant, not a copy of its value.
func TestShutdownGraceFitsInsideTheContainerKillWindow(t *testing.T) {
	const dockerKillGrace = 10 * time.Second
	if shutdownGrace >= dockerKillGrace {
		t.Errorf("shutdownGrace %v must be < the %v container kill grace",
			shutdownGrace, dockerKillGrace)
	}
	if shutdownGrace <= 0 {
		t.Error("shutdownGrace must be positive")
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
