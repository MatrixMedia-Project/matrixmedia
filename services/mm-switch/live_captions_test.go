package main

// Unit tests for the live-captions producer: wire contract, window
// tracker, gating, and finals persistence. No ffmpeg, engine, or network:
// the transcriber is scripted and emission has no viewers.

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// scriptTranscriber replays canned window texts in order, then silence.
type scriptTranscriber struct {
	texts []string
	i     int
}

func (s *scriptTranscriber) Transcribe(_ []byte, t0 float64) (captionChunk, error) {
	if s.i >= len(s.texts) {
		return captionChunk{T0: t0}, nil
	}
	t := s.texts[s.i]
	s.i++
	return captionChunk{T0: t0, Text: t, Language: "en"}, nil
}

func testTap(t *testing.T, script []string) *captionTap {
	t.Helper()
	ms := NewMediaSwitch()
	tap := newCaptionTap(ms, "stream-abcdef01", "rec1",
		&scriptTranscriber{texts: script},
		captionConfig{WindowSecs: 6, ShiftSecs: 3})
	return tap
}

// The wire message must stay parseable by LiveCaptionWireMessage.parse:
// {"type":"caption","segments":[{"id","text","final","language"}]}.
func TestCaptionWireGolden(t *testing.T) {
	msg, err := encodeCaptionBatch([]captionWireSegment{
		{ID: "live-stream", Text: "hello world", Final: false, Language: "en"},
	})
	if err != nil {
		t.Fatal(err)
	}
	var v map[string]any
	if err := json.Unmarshal([]byte(msg), &v); err != nil {
		t.Fatal(err)
	}
	if v["type"] != "caption" {
		t.Fatalf("type = %v", v["type"])
	}
	segs, ok := v["segments"].([]any)
	if !ok || len(segs) != 1 {
		t.Fatalf("segments = %v", v["segments"])
	}
	s0 := segs[0].(map[string]any)
	for k, want := range map[string]any{
		"id": "live-stream", "text": "hello world", "final": false, "language": "en",
	} {
		if s0[k] != want {
			t.Errorf("segments[0][%q] = %v, want %v", k, s0[k], want)
		}
	}
	if _, err := encodeCaptionBatch(nil); err == nil {
		t.Error("empty batch must be refused (client drops it)")
	}
}

// Interim under a stable id; promotion to final after repeat windows.
func TestCaptionTrackerInterimThenFinal(t *testing.T) {
	tap := testTap(t, []string{"hello world", "hello world", "hello world"})
	tap.processWindow(make([]byte, 100), 0) // interim, id live-*
	if tap.pending != "hello world" {
		t.Fatalf("pending = %q", tap.pending)
	}
	if len(tap.finals) != 0 {
		t.Fatalf("finals = %v", tap.finals)
	}
	if !strings.Contains(tap.liveID, "live-") {
		t.Fatalf("liveID = %q, want stable live- prefix", tap.liveID)
	}
	tap.processWindow(make([]byte, 100), 3) // stable=1, still interim
	if len(tap.finals) != 0 {
		t.Fatalf("finals after 2 windows = %v", tap.finals)
	}
	tap.processWindow(make([]byte, 100), 6) // stable=2, promotes
	if len(tap.finals) != 1 {
		t.Fatalf("finals after 3 windows = %v", tap.finals)
	}
	if tap.finals[0].Text != "hello world" {
		t.Errorf("final text = %q", tap.finals[0].Text)
	}
	if tap.pending != "" {
		t.Errorf("pending not cleared after promote: %q", tap.pending)
	}
}

// New text replaces the interim; silence holds; flush files the tail.
func TestCaptionTrackerNewTextAndFlush(t *testing.T) {
	tap := testTap(t, []string{"one two", "three four", ""})
	tap.processWindow(make([]byte, 100), 0)
	tap.processWindow(make([]byte, 100), 3)
	if tap.pending != "three four" {
		t.Fatalf("pending = %q, want replaced interim", tap.pending)
	}
	tap.processWindow(make([]byte, 100), 6) // silence
	if tap.pending != "three four" {
		t.Fatalf("silence must hold the line, pending = %q", tap.pending)
	}
	tap.flushPending()
	if tap.pending != "" || len(tap.finals) != 1 || tap.finals[0].Text != "three four" {
		t.Fatalf("flush must file the tail: pending=%q finals=%v", tap.pending, tap.finals)
	}
}

// Engine client parses verbose_json and stamps absolute times.
func TestEngineTranscriberParsesVerboseJSON(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !strings.Contains(r.Header.Get("Content-Type"), "multipart/form-data") {
			t.Errorf("content-type = %q", r.Header.Get("Content-Type"))
		}
		if err := r.ParseMultipartForm(1 << 20); err != nil {
			t.Errorf("multipart: %v", err)
		}
		if r.FormValue("response_format") != "verbose_json" || r.FormValue("language") != "en" {
			t.Errorf("fields = %q / %q", r.FormValue("response_format"), r.FormValue("language"))
		}
		if r.MultipartForm.File["file"] == nil {
			t.Error("missing file field")
		}
		_, _ = w.Write([]byte(`{"text":" hi there ","language":"en",` +
			`"segments":[{"start":0.5,"end":2.0,"text":" hi there "}]}`))
	}))
	defer srv.Close()
	tr := newEngineTranscriber(newCaptionEngine(captionConfig{}), srv.URL)
	chunk, err := tr.transcribeWindow(make([]byte, 32000), 10)
	if err != nil {
		t.Fatal(err)
	}
	if chunk.Text != "hi there" {
		t.Errorf("text = %q (must be normalised)", chunk.Text)
	}
	if len(chunk.Segments) != 1 || chunk.Segments[0].Start != 10.5 || chunk.Segments[0].End != 12 {
		t.Errorf("segments not stamped absolute: %+v", chunk.Segments)
	}
}

// VTT timestamps + sidecar: exact format, empty finals write nothing.
func TestFormatVTTTime(t *testing.T) {
	for in, want := range map[float64]string{
		0:      "00:00:00.000",
		3.2:    "00:00:03.200",
		61.055: "00:01:01.055",
		3723.4: "01:02:03.400",
		-1:     "00:00:00.000",
	} {
		if got := formatVTTTime(in); got != want {
			t.Errorf("formatVTTTime(%v) = %q, want %q", in, got, want)
		}
	}
}

func TestWriteLiveVTT(t *testing.T) {
	old := liveFinalsDir
	liveFinalsDir = t.TempDir()
	defer func() { liveFinalsDir = old }()

	if key, err := writeLiveVTT("rec0", nil); err != nil || key != "" {
		t.Fatalf("empty finals must write nothing: key=%q err=%v", key, err)
	}
	key, err := writeLiveVTT("rec1", []captionFinal{
		{T0: 0.5, T1: 2, Text: "hello world", Language: "en"},
		{T0: 2, T1: 4.25, Text: "  ", Language: "en"},
		{T0: 4.25, T1: 6, Text: "second line", Language: "en"},
	})
	if err != nil {
		t.Fatal(err)
	}
	raw, err := os.ReadFile(key)
	if err != nil {
		t.Fatal(err)
	}
	want := "WEBVTT\n\n00:00:00.500 --> 00:00:02.000\nhello world\n\n" +
		"00:00:04.250 --> 00:00:06.000\nsecond line\n\n"
	if string(raw) != want {
		t.Errorf("VTT:\n%s\nwant:\n%s", raw, want)
	}
}

// WAV writer: 44-byte header, RIFF/WAVE magic, exact data length.
func TestEncodeWAV(t *testing.T) {
	pcm := make([]byte, 3200)
	out := encodeWAV(pcm)
	if len(out) != 44+len(pcm) {
		t.Fatalf("len = %d", len(out))
	}
	if string(out[:4]) != "RIFF" || string(out[8:16]) != "WAVEfmt " || string(out[36:40]) != "data" {
		t.Errorf("bad WAV header: %q %q %q", out[:4], out[8:16], out[36:40])
	}
}

// Finals persist as NDJSON and read back for finalise.
func TestLiveFinalsRoundtrip(t *testing.T) {
	old := liveFinalsDir
	liveFinalsDir = t.TempDir()
	defer func() { liveFinalsDir = old }()

	tap := testTap(t, nil)
	tap.recording = "rec9"
	tap.openFinalsFile()
	tap.appendFinal(captionFinal{T0: 1, T1: 2, Text: "alpha", Language: "en"})
	tap.appendFinal(captionFinal{T0: 2, T1: 3, Text: "beta", Language: "en"})
	tap.stop()

	got := readLiveFinals("rec9")
	if len(got) != 2 || got[0].Text != "alpha" || got[1].T1 != 3 {
		t.Fatalf("roundtrip = %+v", got)
	}
	if _, err := os.Stat(filepath.Join(liveFinalsDir, "rec9.live.json")); err != nil {
		t.Fatalf("sidecar file: %v", err)
	}
}

// Gating: disabled feature, missing consent, and non-webrtc sources start
// nothing. (The happy path needs a real PeerConnection; covered by E2E.)
type captionFakeSource struct{}

func (captionFakeSource) Type() string                               { return "file" }
func (captionFakeSource) IsActive() bool                             { return true }
func (captionFakeSource) Subscribe(_ string, _ PacketHandler) func() { return func() {} }
func (captionFakeSource) RequestKeyframe()                           {}
func (captionFakeSource) Stop()                                      {}

func TestCaptionGating(t *testing.T) {
	ms := NewMediaSwitch()
	ms.captionCfg.Enabled = false
	ms.AddSource("s1", captionFakeSource{})
	ms.SetCaptionFlag("s1", true)
	ms.MaybeStartCaptionTap("s1", "rec1")
	if len(ms.captionTaps) != 0 {
		t.Fatal("disabled feature must not tap")
	}

	ms.captionCfg.Enabled = true
	ms.MaybeStartCaptionTap("s1", "rec1") // flag on, but non-webrtc source
	if len(ms.captionTaps) != 0 {
		t.Fatal("non-webrtc source must not tap")
	}

	ms.SetCaptionFlag("s2", false)
	if ms.CaptionFlag("s2") {
		t.Fatal("unset consent must read false")
	}
	ms.StopCaptionTap("missing") // no-op, must not panic
}
