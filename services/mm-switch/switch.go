package main

import (
	"fmt"
	"log"
	"sync"
)

// MediaSwitch is the core routing matrix.
// It manages sources (inputs) and viewers (outputs).
// Any viewer can be switched to any source independently.
type MediaSwitch struct {
	mu        sync.RWMutex
	sources   map[string]Source        // sourceID -> Source
	viewers   map[string]*Viewer       // viewerID -> Viewer
	recorders map[string]*WebMRecorder // sourceID -> recording session (one per source)
}

func NewMediaSwitch() *MediaSwitch {
	return &MediaSwitch{
		sources:   make(map[string]Source),
		viewers:   make(map[string]*Viewer),
		recorders: make(map[string]*WebMRecorder),
	}
}

// GetSource returns the Source registered under id (nil if absent).
// Used by the recording handler to attach a tap.
func (ms *MediaSwitch) GetSource(id string) Source {
	ms.mu.RLock()
	defer ms.mu.RUnlock()
	return ms.sources[id]
}

// GetRecorder returns the active recorder for a source (nil if none).
func (ms *MediaSwitch) GetRecorder(sourceID string) *WebMRecorder {
	ms.mu.RLock()
	defer ms.mu.RUnlock()
	return ms.recorders[sourceID]
}

// RegisterRecorder stores a recorder under its source id. Replaces
// any existing recorder for that source (caller's responsibility to
// finalise the old one first).
func (ms *MediaSwitch) RegisterRecorder(sourceID string, rec *WebMRecorder) {
	ms.mu.Lock()
	defer ms.mu.Unlock()
	ms.recorders[sourceID] = rec
}

// UnregisterRecorder drops the recorder from the registry. Does not
// finalise — caller is responsible.
func (ms *MediaSwitch) UnregisterRecorder(sourceID string) {
	ms.mu.Lock()
	defer ms.mu.Unlock()
	delete(ms.recorders, sourceID)
}

// FailRecorderBySubscriber flips the recorder whose fan-out subscriber
// ID ("recorder-{recording_id}") matches subID to the failed state.
// Called from the quarantine path after a recorder handler panicked.
// The recorder stays in the registry so the control plane still sees
// state "failed" on resume/finalise instead of a 404.
func (ms *MediaSwitch) FailRecorderBySubscriber(subID, reason string) {
	var rec *WebMRecorder
	ms.mu.RLock()
	for _, r := range ms.recorders {
		if "recorder-"+r.id == subID {
			rec = r
			break
		}
	}
	ms.mu.RUnlock()
	if rec != nil {
		// Outside ms.mu — markFailed takes the recorder and source locks.
		rec.markFailed(reason, false)
	}
}

// RecorderStats returns recorder counts by state for /health.
func (ms *MediaSwitch) RecorderStats() map[string]int {
	ms.mu.RLock()
	recs := make([]*WebMRecorder, 0, len(ms.recorders))
	for _, r := range ms.recorders {
		recs = append(recs, r)
	}
	ms.mu.RUnlock()
	stats := make(map[string]int, 4)
	for _, r := range recs {
		stats[string(r.State())]++
	}
	return stats
}

// AddSource registers an input source.
func (ms *MediaSwitch) AddSource(id string, src Source) {
	ms.mu.Lock()
	defer ms.mu.Unlock()
	ms.sources[id] = src
	log.Printf("[switch] source added: %s (%s)", id, src.Type())
}

// RemoveSource removes an input source and disconnects viewers using it.
func (ms *MediaSwitch) RemoveSource(id string) {
	ms.removeSource(id, nil)
}

// RemoveSourceIf removes the source registered under `id` ONLY if it is still `want`.
//
// Sources are keyed by a caller-supplied string, and AddSource overwrites without
// checking. So an id can change hands between the moment a caller registers a source and
// the moment it decides to clean up. Removing by id alone would then tear down whoever
// owns the id NOW — including calling DetachSource on their live viewers, which matches
// on the id string, not on the source object.
//
// Any cleanup path that did not itself just create the source it is removing must use
// this, not RemoveSource.
func (ms *MediaSwitch) RemoveSourceIf(id string, want Source) {
	ms.removeSource(id, want)
}

// want == nil means "remove whatever is there".
func (ms *MediaSwitch) removeSource(id string, want Source) {
	ms.mu.Lock()
	defer ms.mu.Unlock()

	src, ok := ms.sources[id]
	if !ok {
		return
	}
	if want != nil && src != want {
		// The id belongs to someone else now. Not ours to remove.
		return
	}
	src.Stop()
	delete(ms.sources, id)

	// Viewers on this source get disconnected (they'll see black until switched)
	for _, v := range ms.viewers {
		if v.CurrentSourceID() == id {
			v.DetachSource()
		}
	}
	log.Printf("[switch] source removed: %s", id)
}

// AddViewer registers a viewer output.
//
// A duplicate id displaces the previous viewer. The displaced one is closed: it owns a
// PeerConnection, a subscription and (in async mode) a writer goroutine, and once it is
// out of the map nothing else holds a reference that could ever shut it down.
func (ms *MediaSwitch) AddViewer(id string, v *Viewer) {
	ms.mu.Lock()
	displaced, existed := ms.viewers[id]
	ms.viewers[id] = v
	ms.mu.Unlock()

	if existed && displaced != v {
		log.Printf("[switch] viewer %s displaced by a new registration; closing the old one", id)
		displaced.Close() // outside ms.mu — Close can block for viewerDrainTimeout
	}
	log.Printf("[switch] viewer added: %s", id)
}

// RemoveViewer removes a viewer.
func (ms *MediaSwitch) RemoveViewer(id string) {
	ms.removeViewer(id, nil)
}

// RemoveViewerIf removes the viewer registered under `id` ONLY if it is still `want`.
// Same hazard as RemoveSourceIf: ids can change hands, and closing the wrong viewer
// kills a live session.
func (ms *MediaSwitch) RemoveViewerIf(id string, want *Viewer) {
	ms.removeViewer(id, want)
}

func (ms *MediaSwitch) removeViewer(id string, want *Viewer) {
	ms.mu.Lock()
	v, ok := ms.viewers[id]
	if !ok || (want != nil && v != want) {
		ms.mu.Unlock()
		return
	}
	delete(ms.viewers, id)
	ms.mu.Unlock()

	// Close OUTSIDE ms.mu. It waits up to viewerDrainTimeout for the writer goroutine,
	// and a wedged viewer is precisely the case that exists — holding the switch lock
	// across it would freeze the entire control plane (publish, health, list) behind one
	// bad client.
	v.Close()
	log.Printf("[switch] viewer removed: %s", id)
}

// SwitchViewer routes a viewer to a different source.
// This is the core operation: seamless source switching per viewer.
func (ms *MediaSwitch) SwitchViewer(viewerID, sourceID string) error {
	ms.mu.RLock()
	defer ms.mu.RUnlock()

	v, ok := ms.viewers[viewerID]
	if !ok {
		return fmt.Errorf("viewer %s not found", viewerID)
	}

	src, ok := ms.sources[sourceID]
	if !ok {
		return fmt.Errorf("source %s not found", sourceID)
	}

	v.SwitchTo(sourceID, src)
	log.Printf("[switch] viewer %s -> source %s", viewerID, sourceID)
	return nil
}

// ListSources returns all registered source IDs.
func (ms *MediaSwitch) ListSources() []SourceInfo {
	ms.mu.RLock()
	defer ms.mu.RUnlock()

	var result []SourceInfo
	for id, src := range ms.sources {
		result = append(result, SourceInfo{
			ID:     id,
			Type:   src.Type(),
			Active: src.IsActive(),
		})
	}
	return result
}

// ListViewers returns all viewer statuses.
func (ms *MediaSwitch) ListViewers() []ViewerInfo {
	ms.mu.RLock()
	defer ms.mu.RUnlock()

	var result []ViewerInfo
	for id, v := range ms.viewers {
		result = append(result, ViewerInfo{
			ID:             id,
			CurrentSource:  v.CurrentSourceID(),
			Connected:      v.IsConnected(),
		})
	}
	return result
}

type SourceInfo struct {
	ID     string `json:"id"`
	Type   string `json:"type"`
	Active bool   `json:"active"`
}

type ViewerInfo struct {
	ID            string `json:"id"`
	CurrentSource string `json:"current_source"`
	Connected     bool   `json:"connected"`
}

// FinaliseAllRecorders flushes and closes every in-progress recording.
//
// Called from the SIGTERM path. Without it, `docker stop` killed the process
// outright: WebMRecorder.Finalise() never ran, buffered frames were lost, and the
// mm_recordings row was left stuck in a non-finalised state (the .webm on disk is
// usually still playable — WebM tolerates streaming writes — but the control-plane
// state was wrong and the recording never became a VOD).
//
// Returns the number of recorders finalised.
func (ms *MediaSwitch) FinaliseAllRecorders() int {
	// Snapshot under the lock; Finalise() takes recorder/source locks of its own,
	// so calling it while holding ms.mu would risk a lock-order inversion.
	ms.mu.RLock()
	pending := make([]*WebMRecorder, 0, len(ms.recorders))
	for _, r := range ms.recorders {
		pending = append(pending, r)
	}
	ms.mu.RUnlock()

	for _, r := range pending {
		r.Finalise()
	}
	return len(pending)
}
