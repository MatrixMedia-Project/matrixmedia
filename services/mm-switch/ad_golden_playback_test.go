// services/mm-switch/ad_golden_playback_test.go
package main

import (
	"encoding/json"
	"math"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/pion/rtp"
)

// Real profile-v1 WebMs (made by mm-media-worker's golden tests with MM_GOLDEN_OUT) play through
// the real FileSource: nothing dropped, the first frame within 1 s of subscribing, and frames
// paced at the file's rate (median gap within ±5 ms).
func TestGoldenAdsPlayThroughTheWebMFileSource(t *testing.T) {
	dir := os.Getenv("MM_GOLDEN_WEBM_DIR")
	if dir == "" {
		t.Skip("MM_GOLDEN_WEBM_DIR not set (see Task 21: MM_GOLDEN_OUT=… just test-media)")
	}
	files, _ := filepath.Glob(filepath.Join(dir, "*.webm"))
	if len(files) == 0 {
		t.Fatal("no golden WebMs in MM_GOLDEN_WEBM_DIR")
	}
	for _, path := range files {
		path := path
		t.Run(filepath.Base(path), func(t *testing.T) {
			var want struct {
				Frames int     `json:"frames"`
				FPS    float64 `json:"fps"`
			}
			raw, err := os.ReadFile(strings.TrimSuffix(path, ".webm") + ".json")
			if err != nil {
				t.Fatal(err)
			}
			if err := json.Unmarshal(raw, &want); err != nil {
				t.Fatal(err)
			}
			src, err := NewFileSource("golden-"+filepath.Base(path), path, false)
			if err != nil {
				t.Fatal(err)
			}
			defer src.Stop()
			var mu sync.Mutex
			var arrivals []time.Time
			start := time.Now()
			src.Subscribe("golden", func(kind string, pkt *rtp.Packet) {
				if kind == "video" && pkt.Marker {
					mu.Lock()
					arrivals = append(arrivals, time.Now())
					mu.Unlock()
				}
			})
			limit := time.Duration(float64(want.Frames)/want.FPS*float64(time.Second)) + 3*time.Second
			waitUntil(t, limit, "every frame played", func() bool {
				mu.Lock()
				defer mu.Unlock()
				return len(arrivals) >= want.Frames-2
			})
			mu.Lock()
			defer mu.Unlock()
			if d := arrivals[0].Sub(start); d > time.Second {
				t.Fatalf("first frame after %v", d)
			}
			gaps := make([]float64, 0, len(arrivals)-1)
			for i := 1; i < len(arrivals); i++ {
				gaps = append(gaps, float64(arrivals[i].Sub(arrivals[i-1]).Microseconds())/1000)
			}
			sort.Float64s(gaps)
			if median, frame := gaps[len(gaps)/2], 1000/want.FPS; math.Abs(median-frame) > 5 {
				t.Fatalf("median frame gap %.1f ms, want %.1f ±5", median, frame)
			}
		})
	}
}
