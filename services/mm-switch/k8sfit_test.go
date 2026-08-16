package main

// K8s-fitness unit tests: shutdown grace from env (B2) and the ICE transport
// config incl. single-port UDP mux mode (B1). See
// WorkingDirectory/docs/k8s-fitness-fixes-2026-08-15.md.

import (
	"net"
	"testing"
	"time"
)

// --- B2: shutdown grace -----------------------------------------------------

func TestShutdownGraceDefault(t *testing.T) {
	t.Setenv("MM_SWITCH_SHUTDOWN_GRACE_SECS", "")
	if got := loadShutdownGrace(); got != 8*time.Second {
		t.Fatalf("default grace = %v, want 8s", got)
	}
}

func TestShutdownGraceFromEnv(t *testing.T) {
	t.Setenv("MM_SWITCH_SHUTDOWN_GRACE_SECS", "30")
	if got := loadShutdownGrace(); got != 30*time.Second {
		t.Fatalf("grace = %v, want 30s", got)
	}
}

func TestShutdownGraceGarbageFallsBack(t *testing.T) {
	t.Setenv("MM_SWITCH_SHUTDOWN_GRACE_SECS", "soon")
	if got := loadShutdownGrace(); got != 8*time.Second {
		t.Fatalf("garbage grace = %v, want default 8s", got)
	}
}

func TestShutdownGraceRejectsNonPositive(t *testing.T) {
	t.Setenv("MM_SWITCH_SHUTDOWN_GRACE_SECS", "0")
	if got := loadShutdownGrace(); got != 8*time.Second {
		t.Fatalf("zero grace = %v, want default 8s", got)
	}
}

// --- B1: ICE config / UDP mux ----------------------------------------------

func TestICEConfigDefaultIsRangeMode(t *testing.T) {
	t.Setenv("MM_SWITCH_UDP_MODE", "")
	t.Setenv("MM_SWITCH_UDP_START", "")
	t.Setenv("MM_SWITCH_UDP_END", "")
	cfg, err := loadICEConfig()
	if err != nil {
		t.Fatalf("loadICEConfig: %v", err)
	}
	if cfg.mode != udpModeRange {
		t.Fatalf("default mode = %q, want range", cfg.mode)
	}
	if cfg.udpStart != 50100 || cfg.udpEnd != 50120 {
		t.Fatalf("default range = %d-%d, want 50100-50120", cfg.udpStart, cfg.udpEnd)
	}
}

func TestICEConfigExplicitRangeEnv(t *testing.T) {
	t.Setenv("MM_SWITCH_UDP_MODE", "range")
	t.Setenv("MM_SWITCH_UDP_START", "40000")
	t.Setenv("MM_SWITCH_UDP_END", "40010")
	cfg, err := loadICEConfig()
	if err != nil {
		t.Fatalf("loadICEConfig: %v", err)
	}
	if cfg.udpStart != 40000 || cfg.udpEnd != 40010 {
		t.Fatalf("range = %d-%d, want 40000-40010", cfg.udpStart, cfg.udpEnd)
	}
}

func TestICEConfigMuxMode(t *testing.T) {
	t.Setenv("MM_SWITCH_UDP_MODE", "mux")
	t.Setenv("MM_SWITCH_UDP_PORT", "0") // 0 = kernel-assigned, safe in tests
	cfg, err := loadICEConfig()
	if err != nil {
		t.Fatalf("loadICEConfig: %v", err)
	}
	if cfg.mode != udpModeMux {
		t.Fatalf("mode = %q, want mux", cfg.mode)
	}
}

func TestICEConfigInvalidModeErrors(t *testing.T) {
	t.Setenv("MM_SWITCH_UDP_MODE", "banana")
	if _, err := loadICEConfig(); err == nil {
		t.Fatal("invalid MM_SWITCH_UDP_MODE must error (fail loud), got nil")
	}
}

func TestMuxBindsAndCloses(t *testing.T) {
	cfg := iceConfig{mode: udpModeMux, muxPort: 0}
	mux, err := openUDPMux(cfg)
	if err != nil {
		t.Fatalf("openUDPMux: %v", err)
	}
	if mux == nil {
		t.Fatal("openUDPMux returned nil mux without error")
	}
	if err := mux.Close(); err != nil {
		t.Fatalf("mux close: %v", err)
	}
}

func TestMuxBindFailureIsLoud(t *testing.T) {
	// Occupy a port, then ask the mux for the same one.
	cfg := iceConfig{mode: udpModeMux, muxPort: 0}
	first, err := openUDPMux(cfg)
	if err != nil {
		t.Fatalf("first bind: %v", err)
	}
	defer first.Close()
	taken := first.LocalAddr().(*net.UDPAddr).Port
	if _, err := openUDPMux(iceConfig{mode: udpModeMux, muxPort: taken}); err == nil {
		t.Fatalf("second bind on port %d must fail loudly, got nil error", taken)
	}
}
