package main

// K8s-fitness knobs (see WorkingDirectory/docs/k8s-fitness-fixes-2026-08-15.md):
//
//   B1  MM_SWITCH_UDP_MODE     "range" (default, today's behavior) | "mux"
//       MM_SWITCH_UDP_PORT     mux-mode single UDP port (default 50100)
//   B2  MM_SWITCH_SHUTDOWN_GRACE_SECS  srv.Shutdown bound (default 8)
//
// Mux mode lets all ICE traffic share ONE UDP port so a Kubernetes Service
// can address the media plane without hostNetwork or a 20-wide hostPort
// range. The mux is process-global: SettingEngine is rebuilt per peer
// connection (see newPeerConnection), but the underlying socket must be
// bound exactly once.

import (
	"fmt"
	"log"
	"net"
	"os"
	"strconv"
	"sync"
	"time"

	"github.com/pion/ice/v4"
	"github.com/pion/webrtc/v4"
)

// defaultShutdownGrace must stay well inside Docker's default 10s kill grace,
// or the process is SIGKILLed mid-flush and in-progress recordings are lost
// anyway — which is the whole thing the graceful path exists to prevent.
// Kubernetes deploys should raise it together with terminationGracePeriodSeconds.
const defaultShutdownGrace = 8 * time.Second

// loadShutdownGrace reads MM_SWITCH_SHUTDOWN_GRACE_SECS. Unset, non-numeric,
// or non-positive values fall back to the default with a log line — a typo'd
// env var must not prevent startup (config hygiene, not correctness).
func loadShutdownGrace() time.Duration {
	raw := os.Getenv("MM_SWITCH_SHUTDOWN_GRACE_SECS")
	if raw == "" {
		return defaultShutdownGrace
	}
	secs, err := strconv.Atoi(raw)
	if err != nil || secs <= 0 {
		log.Printf("MM_SWITCH_SHUTDOWN_GRACE_SECS=%q invalid, using default %s", raw, defaultShutdownGrace)
		return defaultShutdownGrace
	}
	return time.Duration(secs) * time.Second
}

const (
	udpModeRange = "range"
	udpModeMux   = "mux"
)

// iceConfig is the resolved UDP transport configuration.
type iceConfig struct {
	mode     string
	udpStart int // range mode
	udpEnd   int // range mode
	muxPort  int // mux mode
}

// loadICEConfig resolves the UDP transport config from the environment.
// Unknown modes are a hard error: silently falling back to range mode would
// leave a mis-typed K8s deployment publishing the wrong ports.
func loadICEConfig() (iceConfig, error) {
	cfg := iceConfig{mode: udpModeRange, udpStart: 50100, udpEnd: 50120, muxPort: 50100}
	switch m := os.Getenv("MM_SWITCH_UDP_MODE"); m {
	case "", udpModeRange:
		cfg.mode = udpModeRange
	case udpModeMux:
		cfg.mode = udpModeMux
	default:
		return cfg, fmt.Errorf("MM_SWITCH_UDP_MODE=%q: must be %q or %q", m, udpModeRange, udpModeMux)
	}
	if s := os.Getenv("MM_SWITCH_UDP_START"); s != "" {
		fmt.Sscanf(s, "%d", &cfg.udpStart)
	}
	if s := os.Getenv("MM_SWITCH_UDP_END"); s != "" {
		fmt.Sscanf(s, "%d", &cfg.udpEnd)
	}
	if s := os.Getenv("MM_SWITCH_UDP_PORT"); s != "" {
		p, err := strconv.Atoi(s)
		if err != nil || p < 0 || p > 65535 {
			return cfg, fmt.Errorf("MM_SWITCH_UDP_PORT=%q: not a valid port", s)
		}
		cfg.muxPort = p
	}
	return cfg, nil
}

// openUDPMux binds the single mux socket. Bind failure is the caller's fatal
// error — fail loud at startup rather than degrade to a mode the deployment
// didn't publish ports for.
func openUDPMux(cfg iceConfig) (*ice.UDPMuxDefault, error) {
	conn, err := net.ListenUDP("udp", &net.UDPAddr{Port: cfg.muxPort})
	if err != nil {
		return nil, fmt.Errorf("udp mux bind :%d: %w", cfg.muxPort, err)
	}
	return ice.NewUDPMuxDefault(ice.UDPMuxParams{UDPConn: conn}), nil
}

var (
	globalICECfg  iceConfig
	globalUDPMux  *ice.UDPMuxDefault
	iceSetupOnce  sync.Once
	iceSetupError error
)

// setupICETransport resolves the config and (in mux mode) binds the shared
// socket. Called once from main before serving; newPeerConnection consumes
// the result for every subsequent SettingEngine.
func setupICETransport() error {
	iceSetupOnce.Do(func() {
		cfg, err := loadICEConfig()
		if err != nil {
			iceSetupError = err
			return
		}
		globalICECfg = cfg
		if cfg.mode == udpModeMux {
			mux, err := openUDPMux(cfg)
			if err != nil {
				iceSetupError = err
				return
			}
			globalUDPMux = mux
			log.Printf("ICE transport: single-port UDP mux on :%d", mux.LocalAddr().(*net.UDPAddr).Port)
		} else {
			log.Printf("ICE transport: UDP port range %d-%d", cfg.udpStart, cfg.udpEnd)
		}
	})
	return iceSetupError
}

// applyICETransport wires the resolved transport into a per-connection
// SettingEngine.
func applyICETransport(se *webrtc.SettingEngine) {
	if globalICECfg.mode == udpModeMux && globalUDPMux != nil {
		se.SetICEUDPMux(globalUDPMux)
		return
	}
	se.SetEphemeralUDPPortRange(uint16(globalICECfg.udpStart), uint16(globalICECfg.udpEnd)) //nolint:gosec // ports validated
}
