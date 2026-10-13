// services/mm-switch/source_file_start_test.go
package main

import (
	"bytes"
	"encoding/binary"
	"fmt"
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
func adVP8Frame(key bool) []byte {
	if key {
		return append([]byte{0x00, 0x00, 0x00, 0x9d, 0x01, 0x2a, 0x40, 0x00, 0x40, 0x00}, make([]byte, 1100)...)
	}
	return append([]byte{0x01, 0x00, 0x00}, make([]byte, 40)...)
}

// writeTestIVF writes `frames` frames at 30 fps, the first a keyframe. No ffmpeg: CI has none.
func writeTestIVF(t *testing.T, frames int) string {
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
		f := adVP8Frame(i == 0)
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

// Late subscribers read the WebM keyframe cache while playback fills it: under -race this
// fails unless the cache is written under the lock.
func TestWebMKeyframeCacheIsSafeForLateSubscribers(t *testing.T) {
	src, err := NewFileSource("ad-webm", writeTestWebM(t, 45), false)
	if err != nil {
		t.Fatal(err)
	}
	defer src.Stop()
	var first frameCounter
	src.Subscribe("v-first", first.handle) // starts playback, which fills the cache
	var late atomic.Int64
	var wg sync.WaitGroup
	for i := 0; i < 5; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			time.Sleep(time.Duration(40*i) * time.Millisecond)
			src.Subscribe(fmt.Sprintf("v-late-%d", i), func(kind string, _ *rtp.Packet) {
				if kind == "video" {
					late.Add(1)
				}
			})
		}(i)
	}
	wg.Wait()
	waitUntil(t, 3*time.Second, "late subscribers got video", func() bool { return late.Load() > 0 })
	waitUntil(t, 3*time.Second, "the first subscriber got frames", func() bool { return first.count() > 0 })
}
