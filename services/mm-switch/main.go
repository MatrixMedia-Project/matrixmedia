package main

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"sync"
	"sync/atomic"

	"github.com/pion/interceptor"
	"github.com/pion/interceptor/pkg/intervalpli"
	"github.com/pion/webrtc/v4"
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

	apiConfig = webrtc.Configuration{
		ICEServers: []webrtc.ICEServer{
			{URLs: []string{stunServer}},
		},
	}

	mediaSwitch = NewMediaSwitch()

	// HTTP API
	mux := http.NewServeMux()

	// Source management
	mux.HandleFunc("POST /api/sources/file", handleAddFileSource)
	mux.HandleFunc("POST /api/sources/livekit", handleAddLiveKitSource)
	mux.HandleFunc("DELETE /api/sources/{id}", handleRemoveSource)
	mux.HandleFunc("GET /api/sources", handleListSources)

	// Viewer management (WebRTC signaling)
	mux.HandleFunc("POST /api/viewers/offer", handleViewerOffer)
	mux.HandleFunc("DELETE /api/viewers/{id}", handleRemoveViewer)
	mux.HandleFunc("GET /api/viewers", handleListViewers)

	// Publisher (streamer) signaling
	mux.HandleFunc("POST /api/publish/offer", handlePublishOffer)

	// Relay management (LiveKit-to-LiveKit proxy with source switching)
	mux.HandleFunc("POST /api/relay/create", handleCreateRelay)
	mux.HandleFunc("POST /api/relay/switch", handleRelaySwitch)
	mux.HandleFunc("DELETE /api/relay/{id}", handleDeleteRelay)
	mux.HandleFunc("GET /api/relays", handleListRelays)

	// Switching (direct WebRTC viewers)
	mux.HandleFunc("POST /api/switch", handleSwitch)

	// Health
	mux.HandleFunc("GET /health", func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(map[string]any{
			"status":  "ok",
			"sources": len(mediaSwitch.ListSources()),
			"viewers": len(mediaSwitch.ListViewers()),
		})
	})

	log.Printf("[mm-switch] listening on %s", listenAddr)
	log.Printf("[mm-switch] STUN: %s", stunServer)
	log.Fatal(http.ListenAndServe(listenAddr, corsMiddleware(mux)))
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

	api := webrtc.NewAPI(webrtc.WithMediaEngine(m), webrtc.WithInterceptorRegistry(i))
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
