// SPDX-License-Identifier: Apache-2.0
//
// MP4 rendition of finalised recordings. The .webm written by
// WebMRecorder stays the source of truth; transcodeToMP4 derives an
// H.264/AAC faststart MP4 sibling so AVPlayer / ExoPlayer / <video>
// can play VOD natively. Runs async after Finalise (like the
// thumbnail); mm-core polls GET /api/recordings/{id}/mp4 and owns
// the mm_recordings.mp4_status column.

package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"
)

// One ffmpeg transcode at a time — stream-ends are rare and the box
// also relays live media; never fork N x264 encoders concurrently.
var transcodeSem = make(chan struct{}, 1)

var (
	transcodeMu     sync.Mutex
	transcodeStates = map[string]string{}       // recording id → pending|ready|failed
	recordingMeta   = map[string]recMetaEntry{} // recording id → finalised size+duration
)

// recMetaEntry carries the measured size + playback duration of a finalised
// recording so the status endpoint can hand them to mm-core (which persists
// size_bytes / duration_ms — they were NULL because mm-switch never reported
// them and mm-core's mark-ready UPDATE left them unset).
type recMetaEntry struct {
	sizeBytes  int64
	durationMs int64
}

func setTranscodeState(id, st string) {
	transcodeMu.Lock()
	transcodeStates[id] = st
	transcodeMu.Unlock()
}

// setRecordingMeta records the finalised file size + duration for `id`.
func setRecordingMeta(id string, sizeBytes, durationMs int64) {
	transcodeMu.Lock()
	recordingMeta[id] = recMetaEntry{sizeBytes: sizeBytes, durationMs: durationMs}
	transcodeMu.Unlock()
}

// recordingMetaFor returns the finalised size+duration if known.
func recordingMetaFor(id string) (recMetaEntry, bool) {
	transcodeMu.Lock()
	m, ok := recordingMeta[id]
	transcodeMu.Unlock()
	return m, ok
}

// transcodeState resolves via the in-memory map first, then falls
// back to disk so status survives an mm-switch restart.
func transcodeState(id string) string { return transcodeStateIn(recordingsDir, id) }

func transcodeStateIn(dir, id string) string {
	transcodeMu.Lock()
	st, ok := transcodeStates[id]
	transcodeMu.Unlock()
	if ok {
		return st
	}
	base := filepath.Join(dir, id)
	if fi, err := os.Stat(base + ".mp4"); err == nil && fi.Size() > 0 {
		return "ready"
	}
	if _, err := os.Stat(base + ".mp4.part"); err == nil {
		return "pending"
	}
	return "unknown"
}

func transcodeTimeout() time.Duration {
	if v := os.Getenv("MM_TRANSCODE_TIMEOUT_MIN"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			return time.Duration(n) * time.Minute
		}
	}
	return 60 * time.Minute
}

// transcodeToMP4 derives {id}.mp4 from webmPath. Writes to a .part
// temp file and renames on success so nginx never serves a torso.
func transcodeToMP4(id, webmPath string) error {
	mp4Path := strings.TrimSuffix(webmPath, ".webm") + ".mp4"
	tmpPath := mp4Path + ".part"
	setTranscodeState(id, "pending")
	transcodeSem <- struct{}{}
	defer func() { <-transcodeSem }()

	ctx, cancel := context.WithTimeout(context.Background(), transcodeTimeout())
	defer cancel()
	cmd := exec.CommandContext(ctx, "ffmpeg",
		"-loglevel", "error", "-y",
		"-i", webmPath,
		"-c:v", "libx264", "-preset", "veryfast", "-crf", "23",
		"-pix_fmt", "yuv420p",
		"-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2",
		"-c:a", "aac", "-b:a", "128k", "-ar", "48000",
		"-movflags", "+faststart",
		"-f", "mp4",
		tmpPath,
	)
	out, err := cmd.CombinedOutput()
	if err != nil {
		os.Remove(tmpPath)
		setTranscodeState(id, "failed")
		return fmt.Errorf("ffmpeg mp4: %v: %s", err, strings.TrimSpace(string(out)))
	}
	fi, statErr := os.Stat(tmpPath)
	if statErr != nil || fi.Size() == 0 {
		os.Remove(tmpPath)
		setTranscodeState(id, "failed")
		return fmt.Errorf("ffmpeg mp4: produced no output")
	}
	if err := os.Rename(tmpPath, mp4Path); err != nil {
		os.Remove(tmpPath)
		setTranscodeState(id, "failed")
		return fmt.Errorf("ffmpeg mp4: rename: %w", err)
	}
	setTranscodeState(id, "ready")
	log.Printf("[transcode:%s] mp4 ready → %s (%d bytes)", id, mp4Path, fi.Size())
	return nil
}
