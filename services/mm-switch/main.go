package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/pion/interceptor"
	"github.com/pion/interceptor/pkg/intervalpli"
	"github.com/pion/webrtc/v4"
	"github.com/prometheus/client_golang/prometheus/promhttp"
)

var (
	mediaSwitch *MediaSwitch
	viewerCount atomic.Int64
	apiConfig   webrtc.Configuration
	relays      = struct {
		sync.RWMutex
		m map[string]*Relay
	}{m: make(map[string]*Relay)}
)

func main() {
	listenAddr := envOr("MM_SWITCH_LISTEN", ":7890")
	stunServer := envOr("MM_SWITCH_STUN", "stun:stun.l.google.com:19302")
	authSecret := envOr("MM_SWITCH_AUTH_SECRET", "")

	apiConfig = webrtc.Configuration{}
	if stunServer != "" {
		apiConfig.ICEServers = []webrtc.ICEServer{
			{URLs: []string{stunServer}},
		}
	}

	mediaSwitch = NewMediaSwitch()

	// HTTP API
	mux := http.NewServeMux()

	// Auth role sets
	serverOnly := []string{"server"}
	serverPublisher := []string{"server", "publisher"}
	serverViewer := []string{"server", "viewer"}

	// Prometheus metrics — NO auth (scraped by the prometheus container).
	// Counters defined in metrics.go are populated automatically as
	// auth-middleware rejections fire.
	mux.Handle("GET /metrics", promhttp.Handler())

	// Health — NO auth
	mux.HandleFunc("GET /health", func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(map[string]any{
			"status":    "ok",
			"sources":   len(mediaSwitch.ListSources()),
			"viewers":   len(mediaSwitch.ListViewers()),
			"recorders": mediaSwitch.RecorderStats(),
		})
	})

	// GET /api/viewers — NO auth (public viewer count)
	mux.HandleFunc("GET /api/viewers", handleListViewers)

	// POST /api/viewers/offer — auth: server, viewer
	mux.Handle("POST /api/viewers/offer", wrapAuth(authSecret, serverViewer, handleViewerOffer))

	// DELETE /api/viewers/{id} — auth: server
	mux.Handle("DELETE /api/viewers/{id}", wrapAuth(authSecret, serverOnly, handleRemoveViewer))

	// POST /api/publish/offer — auth: server, publisher
	mux.Handle("POST /api/publish/offer", wrapAuth(authSecret, serverPublisher, handlePublishOffer))

	// Source management — auth: server only
	mux.Handle("POST /api/sources/file", wrapAuth(authSecret, serverOnly, handleAddFileSource))
	mux.Handle("POST /api/sources/livekit", wrapAuth(authSecret, serverOnly, handleAddLiveKitSource))
	mux.Handle("DELETE /api/sources/{id}", wrapAuth(authSecret, serverOnly, handleRemoveSource))
	mux.Handle("GET /api/sources", wrapAuth(authSecret, serverOnly, handleListSources))

	// Switching — auth: server only
	mux.Handle("POST /api/switch", wrapAuth(authSecret, serverOnly, handleSwitch))

	// Recording — auth: server only. Single-file-per-session WebM
	// captured by tapping the source's RTP fan-out. start = open-or-
	// resume, pause = freeze write, finalise = close trailer + file.
	// Lifecycle is driven by mm-core (which owns the mm_recordings
	// row) — never by clients directly.
	mux.Handle("POST /api/sources/{id}/record", wrapAuth(authSecret, serverOnly, handleStartOrResumeRecording))
	mux.Handle("DELETE /api/sources/{id}/record", wrapAuth(authSecret, serverOnly, handlePauseRecording))
	mux.Handle("POST /api/sources/{id}/record/finalise", wrapAuth(authSecret, serverOnly, handleFinaliseRecording))
	mux.Handle("GET /api/recordings/{id}/mp4", wrapAuth(authSecret, serverOnly, handleMP4Status))

	// Relay management — auth: server only
	mux.Handle("POST /api/relay/create", wrapAuth(authSecret, serverOnly, handleCreateRelay))
	mux.Handle("POST /api/relay/switch", wrapAuth(authSecret, serverOnly, handleRelaySwitch))
	mux.Handle("DELETE /api/relay/{id}", wrapAuth(authSecret, serverOnly, handleDeleteRelay))
	mux.Handle("GET /api/relays", wrapAuth(authSecret, serverOnly, handleListRelays))

	log.Printf("[mm-switch] listening on %s", listenAddr)
	log.Printf("[mm-switch] STUN: %s", stunServer)
	log.Printf("[mm-switch] recorder isolation: %s (%s)",
		recorderIsolationMode(), recorderIsolationEnv)
	if authSecret != "" {
		log.Printf("[mm-switch] HMAC auth: enabled")
	} else {
		log.Printf("[mm-switch] HMAC auth: disabled (no MM_SWITCH_AUTH_SECRET)")
	}
	// Explicit timeouts. The previous http.ListenAndServe used a zero-value
	// http.Server: no Read/Write/Idle/ReadHeader deadlines at all, which left the
	// service slowloris-exposed and able to accumulate stuck connections forever.
	//
	// These are safe here because every route is a short JSON request/response —
	// media rides WebRTC/UDP, not HTTP — so no long-lived HTTP body is cut short.
	srv := &http.Server{
		Addr:              listenAddr,
		Handler:           corsMiddleware(mux),
		ReadHeaderTimeout: 10 * time.Second,
		ReadTimeout:       30 * time.Second,
		WriteTimeout:      60 * time.Second,
		IdleTimeout:       120 * time.Second,
	}

	// Graceful shutdown. mm-switch had no signal handling whatsoever: `docker stop`
	// (SIGTERM) killed it outright, so WebMRecorder.Finalise() never ran — buffered
	// frames were lost and the mm_recordings row stayed stuck non-finalised, meaning
	// the recording never became a VOD.
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGTERM, syscall.SIGINT)
	defer stop()

	go func() {
		if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Fatalf("[mm-switch] listen failed: %v", err)
		}
	}()
	log.Printf("[mm-switch] listening on %s", listenAddr)

	<-ctx.Done()
	log.Printf("[mm-switch] shutdown signal received; draining")

	// Stop accepting new work first, then flush recordings. The deadline must stay
	// comfortably inside the container's kill grace period (Docker's default is 10s,
	// so keep this well under it) or the process is SIGKILLed mid-flush and we are
	// back where we started.
	shutdownCtx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()

	if err := srv.Shutdown(shutdownCtx); err != nil {
		log.Printf("[mm-switch] http shutdown: %v", err)
	}

	if n := mediaSwitch.FinaliseAllRecorders(); n > 0 {
		log.Printf("[mm-switch] finalised %d in-progress recording(s)", n)
	}
	log.Printf("[mm-switch] shutdown complete")
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

// POST /api/sources/file — Add a file/URL source
func handleAddFileSource(w http.ResponseWriter, r *http.Request) {
	var req struct {
		ID   string `json:"id"`
		Path string `json:"path"` // local file or HTTP URL
		Loop bool   `json:"loop"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), 400)
		return
	}
	if req.ID == "" || req.Path == "" {
		http.Error(w, "id and path required", 400)
		return
	}

	src, err := NewFileSource(req.ID, req.Path, req.Loop)
	if err != nil {
		http.Error(w, err.Error(), 500)
		return
	}

	mediaSwitch.AddSource(req.ID, src)
	jsonReply(w, map[string]string{"id": req.ID, "status": "active"})
}

// POST /api/sources/livekit — Add a LiveKit room as a source (subscribe to streamer's tracks)
func handleAddLiveKitSource(w http.ResponseWriter, r *http.Request) {
	var req struct {
		ID        string `json:"id"`
		URL       string `json:"url"`        // LiveKit server URL (ws://livekit:7880)
		APIKey    string `json:"api_key"`
		APISecret string `json:"api_secret"`
		RoomName  string `json:"room_name"`
		Identity  string `json:"identity"`    // identity for mm-switch in the room
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), 400)
		return
	}
	if req.ID == "" || req.URL == "" || req.RoomName == "" {
		http.Error(w, "id, url, and room_name required", 400)
		return
	}
	if req.Identity == "" {
		req.Identity = "mm-switch-" + req.ID
	}

	src, err := NewLiveKitSource(req.ID, req.URL, req.APIKey, req.APISecret, req.RoomName, req.Identity)
	if err != nil {
		http.Error(w, fmt.Sprintf("failed to connect to LiveKit: %v", err), 500)
		return
	}

	mediaSwitch.AddSource(req.ID, src)
	jsonReply(w, map[string]string{"id": req.ID, "status": "connected"})
}

// DELETE /api/sources/{id} — Remove a source
func handleRemoveSource(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	mediaSwitch.RemoveSource(id)
	jsonReply(w, map[string]string{"ok": "true"})
}

// GET /api/sources — List sources
func handleListSources(w http.ResponseWriter, r *http.Request) {
	jsonReply(w, map[string]any{"sources": mediaSwitch.ListSources()})
}

// POST /api/publish/offer — Publisher (streamer) sends their WebRTC offer
func handlePublishOffer(w http.ResponseWriter, r *http.Request) {
	var req struct {
		ID    string                    `json:"id"`
		Offer webrtc.SessionDescription `json:"offer"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), 400)
		return
	}

	pc, err := createPeerConnection()
	if err != nil {
		http.Error(w, err.Error(), 500)
		return
	}

	src := NewWebRTCSource(req.ID, pc)
	mediaSwitch.AddSource(req.ID, src)

	// Set remote description (publisher's offer)
	if err := pc.SetRemoteDescription(req.Offer); err != nil {
		http.Error(w, err.Error(), 500)
		return
	}

	// Create answer
	answer, err := pc.CreateAnswer(nil)
	if err != nil {
		http.Error(w, err.Error(), 500)
		return
	}

	// Gather ICE candidates
	gatherComplete := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(answer); err != nil {
		http.Error(w, err.Error(), 500)
		return
	}
	<-gatherComplete

	jsonReply(w, map[string]any{
		"id":     req.ID,
		"answer": pc.LocalDescription(),
	})
}

// POST /api/viewers/offer — Viewer sends their WebRTC offer
func handleViewerOffer(w http.ResponseWriter, r *http.Request) {
	var req struct {
		ID       string                    `json:"id"`
		Offer    webrtc.SessionDescription `json:"offer"`
		SourceID string                    `json:"source_id"` // optional: connect to this source immediately
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), 400)
		return
	}

	if req.ID == "" {
		req.ID = fmt.Sprintf("viewer-%d", viewerCount.Add(1))
	}

	pc, err := createPeerConnection()
	if err != nil {
		http.Error(w, err.Error(), 500)
		return
	}

	viewer, err := NewViewer(req.ID, pc)
	if err != nil {
		http.Error(w, err.Error(), 500)
		return
	}

	mediaSwitch.AddViewer(req.ID, viewer)

	// Set remote description (viewer's offer)
	if err := pc.SetRemoteDescription(req.Offer); err != nil {
		http.Error(w, err.Error(), 500)
		return
	}

	// Create answer
	answer, err := pc.CreateAnswer(nil)
	if err != nil {
		http.Error(w, err.Error(), 500)
		return
	}

	gatherComplete := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(answer); err != nil {
		http.Error(w, err.Error(), 500)
		return
	}
	<-gatherComplete

	// If a source_id was specified, connect immediately
	if req.SourceID != "" {
		mediaSwitch.SwitchViewer(req.ID, req.SourceID)
	}

	jsonReply(w, map[string]any{
		"id":     req.ID,
		"answer": pc.LocalDescription(),
	})
}

// DELETE /api/viewers/{id} — Remove viewer
func handleRemoveViewer(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	mediaSwitch.RemoveViewer(id)
	jsonReply(w, map[string]string{"ok": "true"})
}

// GET /api/viewers — List viewers
func handleListViewers(w http.ResponseWriter, r *http.Request) {
	jsonReply(w, map[string]any{"viewers": mediaSwitch.ListViewers()})
}

// POST /api/switch — Switch a viewer to a different source
func handleSwitch(w http.ResponseWriter, r *http.Request) {
	var req struct {
		ViewerID string `json:"viewer_id"`
		SourceID string `json:"source_id"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), 400)
		return
	}

	if err := mediaSwitch.SwitchViewer(req.ViewerID, req.SourceID); err != nil {
		http.Error(w, err.Error(), 404)
		return
	}

	jsonReply(w, map[string]string{"ok": "true", "viewer": req.ViewerID, "source": req.SourceID})
}

// ---------------------------------------------------------------------------
// Recording handlers
// ---------------------------------------------------------------------------

// recordingsDir is the on-disk root for finalised + in-progress
// .webm files. Matches the LiveKit egress mount in docker-compose
// so existing playback URL resolution (`/_mm/recordings/{file}`) keeps
// working.
const recordingsDir = "/data/recordings"

// POST /api/sources/{id}/record — start a new recording for a
// source, OR resume an existing paused one.
//
// Body: { "recording_id": "<uuid>" }    // ignored if resuming
//
// Response: { "id": "...", "state": "recording", "path": "..." }
func handleStartOrResumeRecording(w http.ResponseWriter, r *http.Request) {
	sourceID := r.PathValue("id")
	if sourceID == "" {
		http.Error(w, "source id required", 400)
		return
	}

	// Resume path — recorder already exists.
	if rec := mediaSwitch.GetRecorder(sourceID); rec != nil {
		rec.Resume()
		jsonReply(w, map[string]string{
			"id":    sourceID,
			"state": string(rec.State()),
			"path":  rec.Path(),
		})
		return
	}

	// Start path — need a recording ID for the file name.
	var req struct {
		RecordingID string `json:"recording_id"`
	}
	_ = json.NewDecoder(r.Body).Decode(&req) // optional body
	if req.RecordingID == "" {
		http.Error(w, "recording_id required for new recording", 400)
		return
	}

	src := mediaSwitch.GetSource(sourceID)
	if src == nil {
		http.Error(w, "source not found", 404)
		return
	}
	wsrc, ok := src.(*WebRTCSource)
	if !ok {
		http.Error(w, "source is not a webrtc source — recording only "+
			"supported for direct-publish sources", 400)
		return
	}

	path := filepath.Join(recordingsDir, req.RecordingID+".webm")
	rec, err := NewWebMRecorder(req.RecordingID, path, wsrc)
	if err != nil {
		http.Error(w, err.Error(), 500)
		return
	}
	mediaSwitch.RegisterRecorder(sourceID, rec)
	jsonReply(w, map[string]string{
		"id":    sourceID,
		"state": string(rec.State()),
		"path":  rec.Path(),
	})
}

// DELETE /api/sources/{id}/record — pause the active recording.
// Idempotent.
func handlePauseRecording(w http.ResponseWriter, r *http.Request) {
	sourceID := r.PathValue("id")
	rec := mediaSwitch.GetRecorder(sourceID)
	if rec == nil {
		http.Error(w, "no active recording", 404)
		return
	}
	rec.Pause()
	jsonReply(w, map[string]string{
		"id":    sourceID,
		"state": string(rec.State()),
	})
}

// POST /api/sources/{id}/record/finalise — close the recording and
// drop it from the registry. Called by mm-core on stream end.
//
// Returns the recorder's actual state: "finished" on a clean close,
// "failed" if the recording previously died (panic / write errors) —
// Finalise preserves the failed state so the control plane can stop
// flipping such rows to 'ready'.
func handleFinaliseRecording(w http.ResponseWriter, r *http.Request) {
	sourceID := r.PathValue("id")
	rec := mediaSwitch.GetRecorder(sourceID)
	if rec == nil {
		http.Error(w, "no active recording", 404)
		return
	}
	path := rec.Path()
	rec.Finalise()
	mediaSwitch.UnregisterRecorder(sourceID)
	jsonReply(w, map[string]string{
		"id":    sourceID,
		"state": string(rec.State()),
		"path":  path,
	})
}

// GET /api/recordings/{id}/mp4 — MP4 transcode state for a
// finalised recording. Polled by mm-core, which owns the
// mm_recordings.mp4_status column. Backed by the in-memory
// registry with a disk fallback (survives restarts).
func handleMP4Status(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	if id == "" || strings.ContainsAny(id, "/\\.") {
		http.Error(w, "bad recording id", 400)
		return
	}
	resp := map[string]any{"id": id, "status": transcodeState(id)}
	// Hand mm-core the finalised file size + playback duration so it can
	// persist size_bytes / duration_ms (NULL otherwise — see recMetaEntry).
	if meta, ok := recordingMetaFor(id); ok {
		resp["size_bytes"] = meta.sizeBytes
		resp["duration_ms"] = meta.durationMs
	}
	jsonReply(w, resp)
}

// ---------------------------------------------------------------------------
// Relay handlers
// ---------------------------------------------------------------------------

// POST /api/relay/create — Create a relay: subscribes to a source, publishes into a LiveKit room
func handleCreateRelay(w http.ResponseWriter, r *http.Request) {
	var req struct {
		ID        string `json:"id"`
		SourceID  string `json:"source_id"`  // source to forward
		URL       string `json:"url"`        // LiveKit URL for the relay room
		APIKey    string `json:"api_key"`
		APISecret string `json:"api_secret"`
		RoomName  string `json:"room_name"`  // relay room (viewers connect here)
		Identity  string `json:"identity"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), 400)
		return
	}
	if req.ID == "" || req.URL == "" || req.RoomName == "" {
		http.Error(w, "id, url, and room_name required", 400)
		return
	}
	if req.Identity == "" {
		req.Identity = "mm-relay"
	}

	relay, err := NewRelay(req.ID, req.URL, req.APIKey, req.APISecret, req.RoomName, req.Identity)
	if err != nil {
		http.Error(w, fmt.Sprintf("relay creation failed: %v", err), 500)
		return
	}

	// If source_id specified, switch to it immediately
	if req.SourceID != "" {
		mediaSwitch.mu.RLock()
		if src, ok := mediaSwitch.sources[req.SourceID]; ok {
			relay.SwitchSource(req.SourceID, src)
		}
		mediaSwitch.mu.RUnlock()
	}

	relays.Lock()
	relays.m[req.ID] = relay
	relays.Unlock()

	jsonReply(w, map[string]string{"id": req.ID, "room": req.RoomName, "status": "active"})
}

// POST /api/relay/switch — Switch what a relay is forwarding
func handleRelaySwitch(w http.ResponseWriter, r *http.Request) {
	var req struct {
		RelayID  string `json:"relay_id"`
		SourceID string `json:"source_id"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), 400)
		return
	}

	relays.RLock()
	relay, ok := relays.m[req.RelayID]
	relays.RUnlock()
	if !ok {
		http.Error(w, "relay not found", 404)
		return
	}

	mediaSwitch.mu.RLock()
	src, ok := mediaSwitch.sources[req.SourceID]
	mediaSwitch.mu.RUnlock()
	if !ok {
		http.Error(w, "source not found", 404)
		return
	}

	relay.SwitchSource(req.SourceID, src)
	jsonReply(w, map[string]string{"ok": "true", "relay": req.RelayID, "source": req.SourceID})
}

// DELETE /api/relay/{id} — Delete a relay
func handleDeleteRelay(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	relays.Lock()
	if relay, ok := relays.m[id]; ok {
		relay.Close()
		delete(relays.m, id)
	}
	relays.Unlock()
	jsonReply(w, map[string]string{"ok": "true"})
}

// GET /api/relays — List relays
func handleListRelays(w http.ResponseWriter, r *http.Request) {
	relays.RLock()
	defer relays.RUnlock()
	var list []map[string]string
	for id, relay := range relays.m {
		list = append(list, map[string]string{
			"id":             id,
			"current_source": relay.CurrentSourceID(),
		})
	}
	jsonReply(w, map[string]any{"relays": list})
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

func createPeerConnection() (*webrtc.PeerConnection, error) {
	m := &webrtc.MediaEngine{}
	if err := m.RegisterDefaultCodecs(); err != nil {
		return nil, err
	}

	i := &interceptor.Registry{}
	intervalPliFactory, err := intervalpli.NewReceiverInterceptor()
	if err != nil {
		return nil, err
	}
	i.Add(intervalPliFactory)

	if err := webrtc.RegisterDefaultInterceptors(m, i); err != nil {
		return nil, err
	}

	// Configure ICE with public IP and port range for Docker
	settingEngine := webrtc.SettingEngine{}
	if ip := os.Getenv("MM_SWITCH_PUBLIC_IP"); ip != "" {
		settingEngine.SetNAT1To1IPs([]string{ip}, webrtc.ICECandidateTypeHost)
	}
	udpStart, udpEnd := 50100, 50120
	if s := os.Getenv("MM_SWITCH_UDP_START"); s != "" {
		fmt.Sscanf(s, "%d", &udpStart)
	}
	if s := os.Getenv("MM_SWITCH_UDP_END"); s != "" {
		fmt.Sscanf(s, "%d", &udpEnd)
	}
	settingEngine.SetEphemeralUDPPortRange(uint16(udpStart), uint16(udpEnd))

	// Fast cleanup of dead connections:
	// - Disconnect timeout: 3s (default 5s) — fail faster on lost peer
	// - Fail timeout: 5s (default 25s) — release resources quickly
	// - Keepalive interval: 1s — detect dead peers sooner
	settingEngine.SetICETimeouts(
		3*time.Second,
		5*time.Second,
		1*time.Second,
	)

	api := webrtc.NewAPI(
		webrtc.WithMediaEngine(m),
		webrtc.WithInterceptorRegistry(i),
		webrtc.WithSettingEngine(settingEngine),
	)
	return api.NewPeerConnection(apiConfig)
}

func jsonReply(w http.ResponseWriter, data any) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(data)
}

func corsMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE, OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type, Authorization")
		if r.Method == "OPTIONS" {
			w.WriteHeader(204)
			return
		}
		next.ServeHTTP(w, r)
	})
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}
