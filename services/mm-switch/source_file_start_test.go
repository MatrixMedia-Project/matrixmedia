// services/mm-switch/source_file_start_test.go
package main

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"io"
	"log"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/at-wat/ebml-go/webm"
	"github.com/pion/rtp"
)

// adVP8Frame is a VP8 frame as IVF/WebM store it: a keyframe (P bit clear, start code, 64×64)
// padded past the 1 KB keyframe-cache floor, or a small inter frame.
func adVP8Frame(key bool) []byte { return adVP8FrameSized(key, 1100) }

// adVP8FrameSized is adVP8Frame with the keyframe's padding chosen: under 1 KB it is a keyframe
// the IVF preload (NewFileSource) skips, so playOnce has to fill the keyframe cache itself.
func adVP8FrameSized(key bool, keyPad int) []byte {
	if key {
		return append([]byte{0x00, 0x00, 0x00, 0x9d, 0x01, 0x2a, 0x40, 0x00, 0x40, 0x00}, make([]byte, keyPad)...)
	}
	return append([]byte{0x01, 0x00, 0x00}, make([]byte, 40)...)
}

// writeTestIVF writes `frames` frames at 30 fps, the first a keyframe. No ffmpeg: CI has none.
func writeTestIVF(t *testing.T, frames int) string { return writeIVF(t, frames, 1100) }

// writeIVF is writeTestIVF with the size of the first keyframe's padding chosen (see adVP8FrameSized).
func writeIVF(t *testing.T, frames, keyPad int) string {
	t.Helper()
	var b bytes.Buffer
	hdr := make([]byte, 32)
	copy(hdr[0:4], "DKIF")
	binary.LittleEndian.PutUint16(hdr[6:8], 32)
	copy(hdr[8:12], "VP80")
	binary.LittleEndian.PutUint16(hdr[12:14], 64)
	binary.LittleEndian.PutUint16(hdr[14:16], 64)
	binary.LittleEndian.PutUint32(hdr[16:20], 30) // timebase denominator
	binary.LittleEndian.PutUint32(hdr[20:24], 1)  // timebase numerator
	binary.LittleEndian.PutUint32(hdr[24:28], uint32(frames))
	b.Write(hdr)
	for i := 0; i < frames; i++ {
		f := adVP8FrameSized(i == 0, keyPad)
		fh := make([]byte, 12)
		binary.LittleEndian.PutUint32(fh[0:4], uint32(len(f)))
		binary.LittleEndian.PutUint64(fh[4:12], uint64(i))
		b.Write(fh)
		b.Write(f)
	}
	path := filepath.Join(t.TempDir(), "ad.ivf")
	if err := os.WriteFile(path, b.Bytes(), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

// writeTestWebM writes VP8 + Opus SimpleBlocks the way the recorder does.
func writeTestWebM(t *testing.T, frames int) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "ad.webm")
	f, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	ws, err := webm.NewSimpleBlockWriter(f, []webm.TrackEntry{
		{Name: "Video", TrackNumber: 1, TrackUID: 1, TrackType: 1, CodecID: "V_VP8",
			Video: &webm.Video{PixelWidth: 64, PixelHeight: 64}},
		{Name: "Audio", TrackNumber: 2, TrackUID: 2, TrackType: 2, CodecID: "A_OPUS",
			Audio: &webm.Audio{SamplingFrequency: 48000, Channels: 2}},
	})
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < frames; i++ {
		ms := int64(i) * 1000 / 30
		if _, err := ws[0].Write(i%15 == 0, ms, adVP8Frame(i%15 == 0)); err != nil {
			t.Fatal(err)
		}
		if _, err := ws[1].Write(true, ms, []byte{0xfc, 0xff, 0xfe}); err != nil {
			t.Fatal(err)
		}
	}
	for _, w := range ws {
		w.Close()
	}
	f.Close()
	return path
}

type frameCounter struct {
	mu     sync.Mutex
	frames int
}

func (c *frameCounter) handle(kind string, pkt *rtp.Packet) {
	if kind == "video" && pkt.Marker {
		c.mu.Lock()
		c.frames++
		c.mu.Unlock()
	}
}

func (c *frameCounter) count() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.frames
}

// D11: mm-core registers a pre-roll, then the joining viewer subscribes 1-3 s later (ICE, DTLS).
// The ad must start for that viewer, not at registration — otherwise they miss its opening.
func TestFileSourceStartsAtItsFirstSubscriber(t *testing.T) {
	src, err := NewFileSource("ad-start", writeTestIVF(t, 30), false) // 1 s at 30 fps
	if err != nil {
		t.Fatal(err)
	}
	defer src.Stop()
	time.Sleep(600 * time.Millisecond)
	if src.IsActive() {
		t.Fatal("the ad was playing to nobody before its viewer subscribed")
	}
	var c frameCounter
	src.Subscribe("viewer-1", c.handle)
	// The cached keyframe replayed at once, then all 30 frames from the start.
	waitUntil(t, 3*time.Second, "the whole ad delivered", func() bool { return c.count() >= 31 })
}

// The same start rule for a WebM (the format the converted ads are): idle until the first
// subscriber, then the very first video packet is the keyframe at timestamp 0 and the whole ad
// plays. The first subscriber of a WebM has no cached keyframe to replay (playback fills the cache
// as it starts), so unlike the IVF case there are exactly 30 frames, none replayed.
func TestWebMFileSourceStartsAtItsFirstSubscriber(t *testing.T) {
	src, err := NewFileSource("ad-start-webm", writeTestWebM(t, 30), false) // 1 s at 30 fps
	if err != nil {
		t.Fatal(err)
	}
	defer src.Stop()
	time.Sleep(600 * time.Millisecond)
	if src.IsActive() {
		t.Fatal("the ad was playing to nobody before its viewer subscribed")
	}
	var (
		mu       sync.Mutex
		firstSet bool
		firstKey bool
		firstTs  uint32
		frames   int
	)
	src.Subscribe("viewer-1", func(kind string, pkt *rtp.Packet) {
		if kind != "video" {
			return
		}
		mu.Lock()
		defer mu.Unlock()
		if !firstSet {
			firstSet, firstKey, firstTs = true, IsVP8Keyframe(pkt.Payload), pkt.Timestamp
		}
		if pkt.Marker {
			frames++
		}
	})
	waitUntil(t, 3*time.Second, "the whole ad delivered", func() bool {
		mu.Lock()
		defer mu.Unlock()
		return frames >= 30
	})
	mu.Lock()
	defer mu.Unlock()
	if !firstKey || firstTs != 0 {
		t.Fatalf("the first video packet is key=%v at ts %d, want the keyframe at ts 0", firstKey, firstTs)
	}
}

// quietLog silences the per-Subscribe "replayed cached keyframe" line for the rest of the test:
// the churn tests below subscribe tens of thousands of times.
func quietLog(t *testing.T) {
	t.Helper()
	prev := log.Writer()
	log.SetOutput(io.Discard)
	t.Cleanup(func() { log.SetOutput(prev) })
}

// churnSubscribers has four goroutines subscribe and unsubscribe as fast as they can for d.
// Subscribe reads the keyframe cache under s.mu; this keeps a reader on it every microsecond
// while playback fills it, so an unlocked write cannot hide in a gap between two readers.
func churnSubscribers(src *FileSource, d time.Duration) {
	var wg sync.WaitGroup
	deadline := time.Now().Add(d)
	for g := 0; g < 4; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			id := fmt.Sprintf("churn-%d", g)
			for time.Now().Before(deadline) {
				src.Subscribe(id, func(string, *rtp.Packet) {})()
			}
		}(g)
	}
	wg.Wait()
}

// churnKeyframeCache runs the churn against 20 fresh sources of the file, each right after its
// first Subscribe (which starts playback, and so the cache fill), and then checks that a viewer who
// joins afterwards is handed the cached keyframe by Subscribe itself.
func churnKeyframeCache(t *testing.T, path string) {
	t.Helper()
	quietLog(t)
	for i := 0; i < 20; i++ {
		src, err := NewFileSource(fmt.Sprintf("churn-%d", i), path, false)
		if err != nil {
			t.Fatal(err)
		}
		var first frameCounter
		src.Subscribe("v-first", first.handle)
		churnSubscribers(src, 40*time.Millisecond)
		waitUntil(t, 3*time.Second, "the keyframe cache filled", func() bool {
			src.mu.RLock()
			defer src.mu.RUnlock()
			return len(src.cachedKF) > 0
		})
		var late atomic.Int64
		src.Subscribe("v-late", func(kind string, _ *rtp.Packet) {
			if kind == "video" {
				late.Add(1)
			}
		})
		if late.Load() == 0 { // the replay happens inside Subscribe, before it returns
			t.Fatalf("round %d: a viewer joining after the cache filled was not handed the keyframe", i)
		}
		src.Stop()
	}
}

// Late subscribers read the WebM keyframe cache while playback fills it. Under -race this fails
// with a DATA RACE unless playWebM publishes the cache under s.mu (and Subscribe reads it there).
func TestWebMKeyframeCacheIsSafeForLateSubscribers(t *testing.T) {
	churnKeyframeCache(t, writeTestWebM(t, 45))
}

// The IVF twin. NewFileSource preloads the cache only from a keyframe of at least 1 KB; this
// file's first keyframe is 110 bytes, so playOnce fills the cache while the subscribers read it.
// Under -race this fails with a DATA RACE unless playOnce checks and writes it under s.mu.
func TestIVFKeyframeCacheIsSafeForLateSubscribers(t *testing.T) {
	path := writeIVF(t, 45, 100)
	src, err := NewFileSource("ivf-small-key", path, false)
	if err != nil {
		t.Fatal(err)
	}
	src.mu.RLock()
	preloaded := len(src.cachedKF)
	src.mu.RUnlock()
	src.Stop()
	if preloaded != 0 {
		t.Fatalf("the preload cached a %d-packet keyframe from a sub-1 KB frame: the test no longer reaches playOnce's cache fill", preloaded)
	}
	churnKeyframeCache(t, path)
}
