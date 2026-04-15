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
	mu      sync.RWMutex
	sources map[string]Source  // sourceID -> Source
	viewers map[string]*Viewer // viewerID -> Viewer
}

func NewMediaSwitch() *MediaSwitch {
	return &MediaSwitch{
		sources: make(map[string]Source),
		viewers: make(map[string]*Viewer),
	}
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
	ms.mu.Lock()
	defer ms.mu.Unlock()

	src, ok := ms.sources[id]
	if !ok {
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
func (ms *MediaSwitch) AddViewer(id string, v *Viewer) {
	ms.mu.Lock()
	defer ms.mu.Unlock()
	ms.viewers[id] = v
	log.Printf("[switch] viewer added: %s", id)
}

// RemoveViewer removes a viewer.
func (ms *MediaSwitch) RemoveViewer(id string) {
	ms.mu.Lock()
	defer ms.mu.Unlock()

	v, ok := ms.viewers[id]
	if !ok {
		return
	}
	v.Close()
	delete(ms.viewers, id)
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
