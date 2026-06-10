// SPDX-License-Identifier: Apache-2.0
//
// Flow tests for the MP4 rendition pipeline (transcode.go): synthesize
// a real VP8/Opus WebM (the same codecs WebMRecorder writes), run it
// through transcodeToMP4, and assert codec/faststart properties via
// ffprobe / byte inspection. Tests that need ffmpeg/ffprobe skip
// gracefully when the binaries are absent (they're guaranteed only in
// the container image, Dockerfile:17).

package main

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// makeTestWebM synthesizes a 2-second VP8/Opus WebM via ffmpeg.
func makeTestWebM(t *testing.T, dir string) string {
	t.Helper()
	if _, err := exec.LookPath("ffmpeg"); err != nil {
		t.Skip("ffmpeg not installed")
	}
	out := filepath.Join(dir, "rec_test.webm")
	cmd := exec.Command("ffmpeg", "-loglevel", "error", "-y",
		"-f", "lavfi", "-i", "testsrc2=duration=2:size=320x240:rate=15",
		"-f", "lavfi", "-i", "sine=frequency=440:duration=2",
		"-c:v", "libvpx", "-b:v", "200k",
		"-c:a", "libopus", "-b:a", "64k",
		out)
	if o, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("synthesize webm: %v: %s", err, o)
	}
	return out
}

// TestTranscodeToMP4_H264AACFaststart covers the record→finalise→
// transcode flow at the post-processing seam: a real VP8/Opus WebM in,
// an H.264/AAC faststart MP4 out, atomically published.
func TestTranscodeToMP4_H264AACFaststart(t *testing.T) {
	dir := t.TempDir()
	webmPath := makeTestWebM(t, dir)

	if err := transcodeToMP4("rec_test", webmPath); err != nil {
		t.Fatalf("transcodeToMP4: %v", err)
	}

	mp4Path := filepath.Join(dir, "rec_test.mp4")
	fi, err := os.Stat(mp4Path)
	if err != nil || fi.Size() == 0 {
		t.Fatalf("mp4 missing or empty: %v", err)
	}
	if _, err := os.Stat(mp4Path + ".part"); !os.IsNotExist(err) {
		t.Fatalf(".part temp file left behind: %v", err)
	}
	if st := transcodeStateIn(dir, "rec_test"); st != "ready" {
		t.Fatalf("state = %q, want ready", st)
	}

	// Codec probe — skip the assertion (not the publish checks above)
	// if ffprobe is unavailable.
	if _, err := exec.LookPath("ffprobe"); err != nil {
		t.Skip("ffprobe not installed — skipping codec/faststart assertions")
	}
	out, err := exec.Command("ffprobe", "-v", "error",
		"-show_entries", "stream=codec_name", "-of", "csv=p=0", mp4Path).Output()
	if err != nil {
		t.Fatalf("ffprobe: %v", err)
	}
	codecs := string(out)
	if !strings.Contains(codecs, "h264") || !strings.Contains(codecs, "aac") {
		t.Fatalf("codecs = %q, want h264 + aac", codecs)
	}

	// faststart: moov atom must precede mdat for progressive playback.
	data, err := os.ReadFile(mp4Path)
	if err != nil {
		t.Fatalf("read mp4: %v", err)
	}
	moov, mdat := bytes.Index(data, []byte("moov")), bytes.Index(data, []byte("mdat"))
	if moov < 0 || mdat < 0 || moov >= mdat {
		t.Fatalf("not faststart: moov@%d mdat@%d", moov, mdat)
	}
}

// TestTranscodeToMP4_BadInputMarksFailed covers the failure contract:
// the transcode fails, no .mp4 (or .part) is published, and the state
// reads failed — the WebM stays the only (and served) artifact.
func TestTranscodeToMP4_BadInputMarksFailed(t *testing.T) {
	if _, err := exec.LookPath("ffmpeg"); err != nil {
		t.Skip("ffmpeg not installed")
	}
	dir := t.TempDir()
	badPath := filepath.Join(dir, "rec_bad.webm")
	if err := os.WriteFile(badPath, nil, 0o644); err != nil {
		t.Fatal(err)
	}

	if err := transcodeToMP4("rec_bad", badPath); err == nil {
		t.Fatal("expected error for empty webm input")
	}
	if st := transcodeStateIn(dir, "rec_bad"); st != "failed" {
		t.Fatalf("state = %q, want failed", st)
	}
	for _, leftover := range []string{"rec_bad.mp4", "rec_bad.mp4.part"} {
		if _, err := os.Stat(filepath.Join(dir, leftover)); !os.IsNotExist(err) {
			t.Fatalf("%s should not exist after failure", leftover)
		}
	}
}

// TestTranscodeStateDiskFallback covers restart-survival: ids absent
// from the in-memory map resolve from disk artifacts.
func TestTranscodeStateDiskFallback(t *testing.T) {
	dir := t.TempDir()

	// Published rendition on disk → ready.
	if err := os.WriteFile(filepath.Join(dir, "rec_disk.mp4"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	if st := transcodeStateIn(dir, "rec_disk"); st != "ready" {
		t.Fatalf("state = %q, want ready", st)
	}

	// In-flight .part only → pending.
	if err := os.WriteFile(filepath.Join(dir, "rec_part.mp4.part"), nil, 0o644); err != nil {
		t.Fatal(err)
	}
	if st := transcodeStateIn(dir, "rec_part"); st != "pending" {
		t.Fatalf("state = %q, want pending", st)
	}

	// Nothing on disk, nothing in memory → unknown.
	if st := transcodeStateIn(dir, "rec_nothing"); st != "unknown" {
		t.Fatalf("state = %q, want unknown", st)
	}
}

// TestMP4StatusHandler covers the status endpoint wiring (no auth
// secret, mirroring auth_test.go's pass-through cases).
func TestMP4StatusHandler(t *testing.T) {
	mux := http.NewServeMux()
	mux.Handle("GET /api/recordings/{id}/mp4", wrapAuth("", []string{"server"}, handleMP4Status))

	req := httptest.NewRequest(http.MethodGet, "/api/recordings/nope/mp4", nil)
	rr := httptest.NewRecorder()
	mux.ServeHTTP(rr, req)
	if rr.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", rr.Code)
	}
	var body map[string]string
	if err := json.Unmarshal(rr.Body.Bytes(), &body); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if body["id"] != "nope" || body["status"] != "unknown" {
		t.Fatalf("body = %v, want id=nope status=unknown", body)
	}

	// Path-traversal-ish ids are rejected.
	req = httptest.NewRequest(http.MethodGet, "/api/recordings/evil.webm/mp4", nil)
	rr = httptest.NewRecorder()
	mux.ServeHTTP(rr, req)
	if rr.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400 for id with dot", rr.Code)
	}
}
