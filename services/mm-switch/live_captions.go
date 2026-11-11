package main

// Live captions producer (P2).
//
// When a stream's captions flag is on, a per-stream tap subscribes to the
// programme audio (exactly like WebMRecorder does), forwards raw audio RTP
// packets over UDP loopback to an ffmpeg child (SDP-described, Opus decode
// to 16 kHz mono PCM), slices PCM into overlapping windows, and POSTs each
// window as WAV to a shared whisper-server sidecar. Partial results go out
// immediately as interim segments on each viewing client's mm-control data
// channel; stabilised text is promoted to final segments. Finals are
// appended as NDJSON next to the recording so record/finalise can hand them
// to mm-core for the VOD caption path with no schema change.
//
// Wire format mirrors what iOS already parses (LiveCaptionWireMessage):
//
//	{"type":"caption","segments":[{"id","text","final","language"?}]}
//
// Interim batches reuse one stable id per stream so the client replaces the
// overlay line; finals get monotonic ids so the client files them into
// history. Caps-off streams subscribe to nothing and spawn nothing.

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"math"
	"mime/multipart"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

// captionConfig is read once from the environment at switch boot.
type captionConfig struct {
	// Enabled gates everything: no taps, no engine traffic.
	Enabled bool
	// EngineURL is the mm-live-engine companion (whisper-server /inference).
	EngineURL string
	// WindowSecs is the audio window per inference; ShiftSecs is how far the
	// window advances between inferences (overlap = Window - Shift).
	WindowSecs float64
	ShiftSecs  float64
}

func captionConfigFromEnv() captionConfig {
	return captionConfig{
		Enabled:    os.Getenv("MM_LIVE_CAPTIONS") == "1",
		EngineURL:  envOr("MM_CAPTION_ENGINE_URL", "http://mm-live-engine:8179"),
		WindowSecs: 6,
		ShiftSecs:  3,
	}
}

// envOr lives in main.go; caption defaults flow through it.

// ---------------------------------------------------------------------------
// Wire format (must stay parseable by LiveCaptionWireMessage.parse)
// ---------------------------------------------------------------------------

// captionWireSegment is one overlay line. Language is a BCP-47 tag, empty
// when unknown. Final=false means "interim, replace me".
type captionWireSegment struct {
	ID       string `json:"id"`
	Text     string `json:"text"`
	Final    bool   `json:"final"`
	Language string `json:"language"`
}

// captionWireMessage is the mm-control payload. Type must be exactly
// "caption" — anything else is ignored by the client parser.
type captionWireMessage struct {
	Type     string               `json:"type"`
	Segments []captionWireSegment `json:"segments"`
}

func encodeCaptionBatch(segs []captionWireSegment) (string, error) {
	if len(segs) == 0 {
		return "", fmt.Errorf("live-captions: refusing empty batch (client drops it)")
	}
	msg, err := json.Marshal(captionWireMessage{Type: "caption", Segments: segs})
	if err != nil {
		return "", err
	}
	return string(msg), nil
}

// captionFinal is one persisted line: tap-clock seconds + text. The tap
// clock counts processed audio seconds from tap start, so a tap created
// before the record press runs ahead of the recording; Epoch scopes times
// to one tap lifetime (a toggle off/on restarts the clock).
type captionFinal struct {
	T0       float64 `json:"t0"`
	T1       float64 `json:"t1"`
	Text     string  `json:"text"`
	Language string  `json:"language"`
	Epoch    int     `json:"epoch,omitempty"`
}

// captionRecWindow is one recorded span in tap-clock seconds, scoped to
// the tap lifetime (Epoch) that measured it. Written to the NDJSON sidecar
// as {"window": {...}} when the span closes (pause, toggle-off, finalise),
// so finalise can exclude pre-record and paused speech and shift the rest
// to record-start origin.
type captionRecWindow struct {
	Epoch int     `json:"epoch"`
	Start float64 `json:"start"`
	End   float64 `json:"end"`
}

// ---------------------------------------------------------------------------
// Transcriber: whisper-server over HTTP (verbose_json per window)
// ---------------------------------------------------------------------------

// captionChunk holds one inference window result with stream-relative time.
type captionChunk struct {
	// T0 is the window start in stream seconds; Text is the whole window.
	T0       float64
	Text     string
	Segments []captionTimedSegment
	Language string
}

type captionTimedSegment struct {
	Start, End float64
	Text       string
}

// captionTranscriber turns one WAV window into text. The production
// implementation POSTs to the supervised whisper-server sidecar; tests use
// a fake.
type captionTranscriber interface {
	Transcribe(wav []byte, t0 float64) (captionChunk, error)
}

// engineTranscriber POSTs multipart WAV to whisper-server /inference and
// parses verbose_json. One shared instance serves all taps (the server
// serialises on its model mutex).
type engineTranscriber struct {
	engine *captionEngine
	url    string
	client *http.Client
}

func newEngineTranscriber(engine *captionEngine, url string) *engineTranscriber {
	return &engineTranscriber{
		engine: engine,
		url:    strings.TrimSuffix(url, "/"),
		client: &http.Client{Timeout: 120 * time.Second},
	}
}

type whisperVerboseJSON struct {
	Text     string `json:"text"`
	Language string `json:"language"`
	Segments []struct {
		Start float64 `json:"start"`
		End   float64 `json:"end"`
		Text  string  `json:"text"`
	} `json:"segments"`
}

// transcribeWindow sends one window and stamps it with its stream offset.
func (t *engineTranscriber) transcribeWindow(wav []byte, t0 float64) (captionChunk, error) {
	var body bytes.Buffer
	w := multipart.NewWriter(&body)
	fw, err := w.CreateFormFile("file", "chunk.wav")
	if err != nil {
		return captionChunk{}, err
	}
	if _, err := fw.Write(wav); err != nil {
		return captionChunk{}, err
	}
	_ = w.WriteField("response_format", "verbose_json")
	_ = w.WriteField("language", "en")
	if err := w.Close(); err != nil {
		return captionChunk{}, err
	}
	req, err := http.NewRequest("POST", t.url+"/inference", &body)
	if err != nil {
		return captionChunk{}, err
	}
	req.Header.Set("Content-Type", w.FormDataContentType())
	resp, err := t.client.Do(req)
	if err != nil {
		return captionChunk{}, err
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return captionChunk{}, err
	}
	if resp.StatusCode != 200 {
		return captionChunk{}, fmt.Errorf("live-captions: engine %d: %.120s", resp.StatusCode, raw)
	}
	var vj whisperVerboseJSON
	if err := json.Unmarshal(raw, &vj); err != nil {
		return captionChunk{}, fmt.Errorf("live-captions: bad engine JSON: %w", err)
	}
	out := captionChunk{T0: t0, Text: normCaptionText(vj.Text), Language: vj.Language}
	for _, s := range vj.Segments {
		if normCaptionText(s.Text) == "" {
			continue
		}
		out.Segments = append(out.Segments, captionTimedSegment{
			Start: t0 + s.Start, End: t0 + s.End, Text: normCaptionText(s.Text),
		})
	}
	if out.Language == "" {
		out.Language = "en"
	}
	return out, nil
}

// normCaptionText collapses whitespace so window overlap compares stable.
func normCaptionText(s string) string {
	return strings.Join(strings.Fields(s), " ")
}

// encodeWAV writes 16 kHz mono s16le PCM as a WAV file in memory.
func encodeWAV(pcm []byte) []byte {
	var hdr bytes.Buffer
	// RIFF header (44 bytes, PCM).
	hdr.WriteString("RIFF")
	_ = binary.Write(&hdr, binary.LittleEndian, uint32(36+len(pcm)))
	hdr.WriteString("WAVEfmt ")
	_ = binary.Write(&hdr, binary.LittleEndian, uint32(16))
	_ = binary.Write(&hdr, binary.LittleEndian, uint16(1)) // PCM
	_ = binary.Write(&hdr, binary.LittleEndian, uint16(1)) // mono
	_ = binary.Write(&hdr, binary.LittleEndian, uint32(16000))
	_ = binary.Write(&hdr, binary.LittleEndian, uint32(16000*2))
	_ = binary.Write(&hdr, binary.LittleEndian, uint16(2))
	_ = binary.Write(&hdr, binary.LittleEndian, uint16(16))
	hdr.WriteString("data")
	_ = binary.Write(&hdr, binary.LittleEndian, uint32(len(pcm)))
	return append(hdr.Bytes(), pcm...)
}

// ---------------------------------------------------------------------------
// Engine: whisper-server over HTTP. It runs as the mm-live-engine companion
// container (compose supervises it: restart unless-stopped); the switch
// only dial-checks before POSTing. Error means "skip this window".
// ---------------------------------------------------------------------------

// captionEngine is a dial-check handle, not a supervisor: the process lives
// in the companion container, so a dead engine is a deploy/ops event, not
// something the switch reaps and reforks.
type captionEngine struct {
	cfg captionConfig
}

func newCaptionEngine(cfg captionConfig) *captionEngine {
	return &captionEngine{cfg: cfg}
}

// ensure dials the engine; error means "skip this window, try the next".
func (e *captionEngine) ensure() error {
	c, err := net.DialTimeout("tcp", e.addr(), 2*time.Second)
	if err != nil {
		return fmt.Errorf("live-captions: engine %s unreachable: %w", e.addr(), err)
	}
	_ = c.Close()
	return nil
}

func (e *captionEngine) addr() string {
	u := strings.TrimSuffix(e.cfg.EngineURL, "/")
	u = strings.TrimPrefix(u, "http://")
	u = strings.TrimPrefix(u, "https://")
	if host, port, err := net.SplitHostPort(u); err == nil && port != "" {
		return net.JoinHostPort(host, port)
	}
	return net.JoinHostPort(u, "8179")
}

// ---------------------------------------------------------------------------
// Per-stream tap: audio RTP -> ffmpeg -> windows -> engine -> viewers
// ---------------------------------------------------------------------------

// captionPCMBytesPerSec is 16 kHz mono s16le: 32000 bytes per second.
const captionPCMBytesPerSec = 16000 * 2

// captionTap is one captioned stream's pipeline. All window processing runs
// in a single goroutine (pcmLoop), so tracker state needs no lock beyond
// stop-safety for the finals file.
type captionTap struct {
	ms         *MediaSwitch
	sourceID   string
	recording  string // recording id for finals persistence; "" = live-only
	liveID     string // stable interim segment id
	transcribe captionTranscriber
	window     int // PCM bytes per inference window
	shift      int // PCM bytes the window advances per inference

	ctx    context.Context
	cancel context.CancelFunc
	unsub  func()

	mu          sync.Mutex
	enoughAudio bool // ffmpeg child up and first packets forwarded
	finals      []captionFinal
	finalSeq    int
	pending     string  // current interim text (normalised)
	pendingT0   float64 // == pendingT1 until the text repeats
	pendingT1   float64
	stable      int // consecutive windows reproducing pending
	epoch       int // tap lifetime id; clocks restart per tap
	clockSec    float64
	// Open recording window in tap-clock seconds; live overlay needs no
	// window, so a live-only tap has winOpen == false.
	winOpen    bool
	winStart   float64
	finalsFile *os.File
	fileMu     sync.Mutex

	ffmpeg     *exec.Cmd
	ffmpegPort int
	sdpPath    string
	udpConn    *net.UDPConn
}

// udpWrite forwards one RTP packet; UDP loopback never blocks.
func (t *captionTap) udpWrite(raw []byte) {
	t.mu.Lock()
	udp := t.udpConn
	t.mu.Unlock()
	if udp == nil {
		return
	}
	_, _ = udp.Write(raw)
}

// ---------------------------------------------------------------------------
// Window processing: PCM -> engine -> interim/final -> viewers + finals file
// ---------------------------------------------------------------------------

// pcmLoop reads decoder PCM and processes sliding windows sequentially.
// Sequential POSTs are the backpressure: a slow engine delays windows
// instead of queueing them.
func (t *captionTap) pcmLoop(stdout io.Reader) {
	defer t.stopFFmpeg()
	var pcm []byte
	processed := 0
	buf := make([]byte, 32000)
	for {
		select {
		case <-t.ctx.Done():
			t.flushPending()
			return
		default:
		}
		n, err := stdout.Read(buf)
		if n > 0 {
			pcm = append(pcm, buf[:n]...)
		}
		for len(pcm)-processed >= t.window {
			win := pcm[processed : processed+t.window]
			t0 := float64(processed) / captionPCMBytesPerSec
			processed += t.shift
			t.processWindow(win, t0)
		}
		// Bound memory when the engine stalls: keep the last two windows.
		if excess := len(pcm) - processed - 2*t.window; excess > 0 {
			pcm = append([]byte(nil), pcm[len(pcm)-2*t.window:]...)
			processed = 0
		}
		if err != nil {
			// Decoder exited (stop or crash): file the tail, then end.
			t.flushPending()
			return
		}
	}
}

// processWindow transcribes one window and folds it into interim/finals.
func (t *captionTap) processWindow(pcm []byte, t0 float64) {
	chunk, err := t.transcribe.Transcribe(encodeWAV(pcm), t0)
	if err != nil {
		log.Printf("[live-captions:%s] window @%.0fs skipped: %v", t.sourceID, t0, err)
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	text := chunk.Text
	lang := chunk.Language
	if lang == "" {
		lang = "en"
	}
	dur := float64(len(pcm)) / captionPCMBytesPerSec
	t.clockSec = t0 + dur
	b0, b1 := finalBounds(chunk, t0, dur)
	switch {
	case text == "":
		// Silence: keep the last line up (clients hold state). A later
		// final promotion clears it when speech ends (see flushPending).
		t.stable = 0
	case text == t.pending:
		t.stable++
		t.pendingT1 = b1
		if t.stable >= 2 {
			t.promoteFinalLocked(lang)
		} else {
			t.emitInterimLocked(text, lang)
		}
	default:
		t.pending = text
		t.pendingT0, t.pendingT1 = b0, b1
		t.stable = 0
		t.emitInterimLocked(text, lang)
	}
}

// finalBounds prefers the engine's segment timestamps (absolute); falls
// back to the window span when the engine returns bare text.
func finalBounds(chunk captionChunk, t0, dur float64) (float64, float64) {
	if len(chunk.Segments) > 0 {
		return chunk.Segments[0].Start, chunk.Segments[len(chunk.Segments)-1].End
	}
	return t0, t0 + dur
}

// promoteFinalLocked files the stable pending text as a final line.
func (t *captionTap) promoteFinalLocked(lang string) {
	if t.pending == "" {
		return
	}
	id := fmt.Sprintf("s-%s-%d", shortID(t.sourceID), t.finalSeq)
	t.finalSeq++
	f := captionFinal{T0: t.pendingT0, T1: t.pendingT1, Text: t.pending, Language: lang, Epoch: t.epoch}
	t.finals = append(t.finals, f)
	t.appendFinal(f)
	seg := captionWireSegment{ID: id, Text: t.pending, Final: true, Language: lang}
	t.pending = ""
	t.stable = 0
	t.emitLocked([]captionWireSegment{seg})
}

// emitInterimLocked publishes the current window text under the stable
// interim id so clients replace (not append) the overlay line.
func (t *captionTap) emitInterimLocked(text, lang string) {
	t.emitLocked([]captionWireSegment{{ID: t.liveID, Text: text, Final: false, Language: lang}})
}

func (t *captionTap) emitLocked(segs []captionWireSegment) {
	msg, err := encodeCaptionBatch(segs)
	if err != nil {
		return
	}
	t.ms.emitCaptionBatch(t.sourceID, msg)
}

// flushPending promotes any leftover interim to final (speech ended or tap
// stopping) so the VOD transcript keeps the tail.
func (t *captionTap) flushPending() {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.pending == "" {
		return
	}
	id := fmt.Sprintf("s-%s-%d", shortID(t.sourceID), t.finalSeq)
	t.finalSeq++
	f := captionFinal{T0: t.pendingT0, T1: t.pendingT1, Text: t.pending, Language: "en", Epoch: t.epoch}
	t.finals = append(t.finals, f)
	t.appendFinal(f)
	if msg, err := encodeCaptionBatch([]captionWireSegment{{ID: id, Text: f.Text, Final: true, Language: "en"}}); err == nil {
		t.ms.emitCaptionBatch(t.sourceID, msg)
	}
	t.pending = ""
}

// appendFinal persists one final line as NDJSON for the finalise path.
func (t *captionTap) appendFinal(f captionFinal) {
	raw, err := json.Marshal(f)
	if err != nil {
		return
	}
	t.fileMu.Lock()
	defer t.fileMu.Unlock()
	if t.finalsFile != nil {
		_, _ = t.finalsFile.Write(append(raw, '\n'))
	}
}

// attachRecording binds the tap to a recording and opens a recording
// window at the current tap clock. No-op for a live-only tap ("").
// Idempotent: re-attaching the same recording (resume) only opens a
// window when none is open, so pre-record speech is never saved.
func (t *captionTap) attachRecording(recordingID string) {
	if recordingID == "" {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	t.recording = recordingID
	t.openFinalsFile()
	if !t.winOpen {
		t.winOpen = true
		t.winStart = t.clockSec
	}
}

// pauseRecording closes the open recording window, if any. The tap keeps
// running so the live overlay continues while recording is paused.
func (t *captionTap) pauseRecording() {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.closeWindowLocked(t.clockSec)
}

// resubscribe moves the tap to a replaced source (re-publish under the
// same id): the old subscription is dropped, the new one installed. The
// ffmpeg child and tap clock survive, so windows stay continuous.
func (t *captionTap) resubscribe(wsrc *WebRTCSource) {
	t.mu.Lock()
	unsub := t.unsub
	t.unsub = nil
	t.mu.Unlock()
	if unsub != nil {
		unsub()
	}
	t.mu.Lock()
	t.unsub = wsrc.Subscribe("captions-"+t.sourceID, t.onPacket)
	t.mu.Unlock()
}

// closeWindowLocked files the open window as a closed span. The finalise
// path only saves finals covered by a closed window.
func (t *captionTap) closeWindowLocked(now float64) {
	if !t.winOpen {
		return
	}
	t.winOpen = false
	t.appendWindow(captionRecWindow{Epoch: t.epoch, Start: t.winStart, End: now})
}

// appendWindow persists one closed recording window for the finalise path.
func (t *captionTap) appendWindow(w captionRecWindow) {
	raw, err := json.Marshal(struct {
		Window captionRecWindow `json:"window"`
	}{Window: w})
	if err != nil {
		return
	}
	t.fileMu.Lock()
	defer t.fileMu.Unlock()
	if t.finalsFile != nil {
		_, _ = t.finalsFile.Write(append(raw, '\n'))
	}
}

// openFinalsFile creates the NDJSON sidecar next to the recording.
func (t *captionTap) openFinalsFile() {
	t.fileMu.Lock()
	defer t.fileMu.Unlock()
	if t.recording == "" || t.finalsFile != nil {
		return
	}
	f, err := os.OpenFile(liveFinalsPath(t.recording), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0644)
	if err != nil {
		log.Printf("[live-captions:%s] finals file unavailable: %v", t.sourceID, err)
		return
	}
	t.finalsFile = f
}

// liveFinalsDir defaults to the switch recording root so finals land next
// to the .webm; tests point it at a temp dir.
var liveFinalsDir = recordingsDir

// liveFinalsPath is the NDJSON sidecar next to the .webm.
func liveFinalsPath(recordingID string) string {
	return filepath.Join(liveFinalsDir, recordingID+".live.json")
}

// formatVTTTime renders stream seconds as a WebVTT timestamp.
func formatVTTTime(secs float64) string {
	if secs < 0 {
		secs = 0
	}
	ms := int(secs*1000 + 0.5)
	h, ms := ms/3600000, ms%3600000
	m, ms := ms/60000, ms%60000
	s, ms := ms/1000, ms%1000
	return fmt.Sprintf("%02d:%02d:%02d.%03d", h, m, s, ms)
}

// writeLiveVTT renders finals as a WebVTT sidecar next to the recording and
// returns its storage key. Skipped (empty key) when there are no finals.
func writeLiveVTT(recordingID string, finals []captionFinal) (string, error) {
	if len(finals) == 0 {
		return "", nil
	}
	var b strings.Builder
	b.WriteString("WEBVTT\n\n")
	for _, f := range finals {
		if normCaptionText(f.Text) == "" {
			continue
		}
		fmt.Fprintf(&b, "%s --> %s\n%s\n\n", formatVTTTime(f.T0), formatVTTTime(f.T1), normCaptionText(f.Text))
	}
	key := strings.TrimSuffix(liveFinalsPath(recordingID), ".live.json") + ".en.vtt"
	if err := os.WriteFile(key, []byte(b.String()), 0644); err != nil {
		return "", err
	}
	return key, nil
}

// readLiveFinals loads persisted finals for the finalise response.
// Window marker lines are skipped (see readLiveCaptions).
func readLiveFinals(recordingID string) []captionFinal {
	finals, _ := readLiveCaptions(recordingID)
	return finals
}

// readLiveCaptions loads persisted finals plus closed recording windows
// for the finalise path. Pre-window lines (no epoch) belong to epoch 0.
func readLiveCaptions(recordingID string) ([]captionFinal, []captionRecWindow) {
	raw, err := os.ReadFile(liveFinalsPath(recordingID))
	if err != nil || len(raw) == 0 {
		return nil, nil
	}
	var finals []captionFinal
	var windows []captionRecWindow
	for _, line := range bytes.Split(raw, []byte{'\n'}) {
		line = bytes.TrimSpace(line)
		if len(line) == 0 {
			continue
		}
		var probe struct {
			Window json.RawMessage `json:"window"`
		}
		if err := json.Unmarshal(line, &probe); err != nil {
			continue
		}
		if len(probe.Window) > 0 {
			var w captionRecWindow
			if err := json.Unmarshal(probe.Window, &w); err != nil {
				continue
			}
			windows = append(windows, w)
			continue
		}
		var f captionFinal
		if err := json.Unmarshal(line, &f); err != nil {
			continue
		}
		finals = append(finals, f)
	}
	return finals, windows
}

// mapFinalsToWindows keeps only the finals covered by a closed recording
// window, clips partial overlaps, and shifts every kept cue so the VTT
// starts at record-start origin with paused spans removed. Epochs render
// in order (a toggle off/on restarts the tap clock under a new epoch).
func mapFinalsToWindows(finals []captionFinal, windows []captionRecWindow) []captionFinal {
	byEpoch := make(map[int][]captionRecWindow)
	var epochs []int
	for _, w := range windows {
		if _, ok := byEpoch[w.Epoch]; !ok {
			epochs = append(epochs, w.Epoch)
		}
		byEpoch[w.Epoch] = append(byEpoch[w.Epoch], w)
	}
	sort.Ints(epochs)
	var out []captionFinal
	var base float64
	for _, e := range epochs {
		ws := byEpoch[e]
		sort.Slice(ws, func(i, j int) bool { return ws[i].Start < ws[j].Start })
		for _, w := range ws {
			if w.End <= w.Start {
				continue
			}
			for _, f := range finals {
				if f.Epoch != e || normCaptionText(f.Text) == "" {
					continue
				}
				s := math.Max(f.T0, w.Start)
				en := math.Min(f.T1, w.End)
				if en <= s {
					continue
				}
				out = append(out, captionFinal{
					T0: s - w.Start + base, T1: en - w.Start + base,
					Text: f.Text, Language: f.Language, Epoch: e,
				})
			}
			base += w.End - w.Start
		}
	}
	return out
}

// renderLiveVTT maps persisted finals through the recording windows and
// writes the VTT sidecar. Empty (no key) when nothing was recorded while
// opted in — the VOD worker path then transcribes instead.
func renderLiveVTT(recordingID string) (string, error) {
	finals, windows := readLiveCaptions(recordingID)
	return writeLiveVTT(recordingID, mapFinalsToWindows(finals, windows))
}

// stop cancels the tap: subscription removed, ffmpeg killed, SDP removed,
// finals file closed (content stays for finalise).
func (t *captionTap) stop() {
	t.cancel()
	t.mu.Lock()
	unsub := t.unsub
	t.unsub = nil
	t.mu.Unlock()
	if unsub != nil {
		unsub()
	}
	t.stopFFmpeg()
	t.fileMu.Lock()
	if t.finalsFile != nil {
		_ = t.finalsFile.Close()
		t.finalsFile = nil
	}
	t.fileMu.Unlock()
}

// ---------------------------------------------------------------------------
// MediaSwitch wiring: consent flags, tap lifecycle, viewer emission
// ---------------------------------------------------------------------------

// SetCaptionFlag records the streamer's consent for a source. Called by
// mm-core at stream start, on record-start (with the effective flag), and
// on mid-stream toggle via POST /api/sources/{id}/captions. Consent that
// arrives before publish is stored; the tap attaches on source registration.
func (ms *MediaSwitch) SetCaptionFlag(sourceID string, on bool) {
	ms.mu.Lock()
	defer ms.mu.Unlock()
	if ms.captionFlags == nil {
		ms.captionFlags = make(map[string]bool)
	}
	ms.captionFlags[sourceID] = on
}

// CaptionFlag reports the recorded consent (false when unknown).
func (ms *MediaSwitch) CaptionFlag(sourceID string) bool {
	ms.mu.RLock()
	defer ms.mu.RUnlock()
	return ms.captionFlags[sourceID]
}

// AttachCaptionRecording ensures the live pipeline for an opted-in source
// and binds it to a recording ("" = live-only: overlay runs, nothing is
// saved). The tap starts at publish/launch when consent is known, so live
// captions never wait for the record press; the recording window opens at
// the current tap clock, so pre-record speech stays live-only. Idempotent;
// no-ops for non-webrtc sources (same restriction as recording).
func (ms *MediaSwitch) AttachCaptionRecording(sourceID, recordingID string) {
	if !ms.captionCfg.Enabled || !ms.CaptionFlag(sourceID) {
		return
	}
	ms.mu.Lock()
	tap, ok := ms.captionTaps[sourceID]
	var wsrc *WebRTCSource
	if !ok {
		src, sok := ms.sources[sourceID]
		if !sok {
			ms.mu.Unlock()
			log.Printf("[live-captions:%s] no such source, tap not attached", sourceID)
			return
		}
		var wok bool
		wsrc, wok = src.(*WebRTCSource)
		if !wok {
			ms.mu.Unlock()
			log.Printf("[live-captions:%s] tap needs a webrtc source, not %T", sourceID, src)
			return
		}
		engine := ms.captionEngine
		if engine == nil {
			engine = newCaptionEngine(ms.captionCfg)
			ms.captionEngine = engine
		}
		tr := newEngineTranscriber(engine, ms.captionCfg.EngineURL)
		tap = newCaptionTap(ms, sourceID, "", tr, ms.captionCfg)
		tap.epoch = ms.nextCaptionEpochLocked(sourceID)
		if ms.captionTaps == nil {
			ms.captionTaps = make(map[string]*captionTap)
		}
		ms.captionTaps[sourceID] = tap
	}
	ms.mu.Unlock()
	if wsrc != nil {
		unsub := wsrc.Subscribe("captions-"+sourceID, tap.onPacket)
		tap.mu.Lock()
		tap.unsub = unsub
		tap.mu.Unlock()
		log.Printf("[live-captions:%s] tap attached (epoch %d)", sourceID, tap.epoch)
	}
	tap.attachRecording(recordingID)
}

// nextCaptionEpochLocked hands out the next tap lifetime id for a source.
// A toggle off/on restarts the tap clock, so each lifetime needs its own
// epoch for the finalise path to map times.
func (ms *MediaSwitch) nextCaptionEpochLocked(sourceID string) int {
	if ms.captionEpochs == nil {
		ms.captionEpochs = make(map[string]int)
	}
	ms.captionEpochs[sourceID]++
	return ms.captionEpochs[sourceID]
}

// MaybeStartCaptionTap attaches the live pipeline when the feature is
// enabled globally and the streamer opted in. Kept for the recording
// handlers' call shape; AttachCaptionRecording is the same behavior with
// explicit recording binding.
func (ms *MediaSwitch) MaybeStartCaptionTap(sourceID, recordingID string) {
	ms.AttachCaptionRecording(sourceID, recordingID)
}

// PauseCaptionRecording closes the tap's open recording window, if any.
// The tap keeps running so the live overlay continues while paused.
func (ms *MediaSwitch) PauseCaptionRecording(sourceID string) {
	ms.mu.RLock()
	tap, ok := ms.captionTaps[sourceID]
	ms.mu.RUnlock()
	if ok && tap != nil {
		tap.pauseRecording()
	}
}

// attachCaptionIfOptedIn attaches (or re-attaches) the tap after source
// registration. Consent pushed before publish has nowhere to go until the
// source exists; a re-publish (same id, new object, e.g. after a watchdog
// restart) must move the existing tap's subscription or captions die with
// the replaced source.
func (ms *MediaSwitch) attachCaptionIfOptedIn(id string) {
	if !ms.captionCfg.Enabled || !ms.CaptionFlag(id) {
		return
	}
	ms.mu.RLock()
	tap, hasTap := ms.captionTaps[id]
	src, hasSrc := ms.sources[id]
	var recordingID string
	if rec := ms.recorders[id]; rec != nil {
		recordingID = rec.ID()
	}
	ms.mu.RUnlock()
	wsrc, ok := src.(*WebRTCSource)
	if !ok || !hasSrc {
		return
	}
	if hasTap && tap != nil {
		tap.resubscribe(wsrc)
		tap.attachRecording(recordingID)
		return
	}
	ms.AttachCaptionRecording(id, recordingID)
}

// StopCaptionTap detaches the pipeline; persisted finals and closed
// windows stay for finalise. The tail is flushed synchronously inside the
// still-open window so toggle-off keeps the recorded span to the toggle.
func (ms *MediaSwitch) StopCaptionTap(sourceID string) {
	ms.mu.Lock()
	tap, ok := ms.captionTaps[sourceID]
	if ok {
		delete(ms.captionTaps, sourceID)
	}
	ms.mu.Unlock()
	if ok && tap != nil {
		tap.flushPending()
		tap.pauseRecording()
		tap.stop()
		log.Printf("[live-captions:%s] tap stopped", sourceID)
	}
}

// emitCaptionBatch best-effort-sends a caption message to every viewer of
// the source whose mm-control channel is open. Mirrors the codec-signal
// pattern: silent on a dead channel, unknown types ignored by old builds.
func (ms *MediaSwitch) emitCaptionBatch(sourceID, msg string) {
	for _, v := range ms.ViewerObjects() {
		if v.CurrentSourceID() != sourceID {
			continue
		}
		v.sendCaptionText(msg)
	}
}

// sendCaptionText is the caption analogue of the codec signal: best effort,
// never blocks media.
func (v *Viewer) sendCaptionText(msg string) {
	v.mu.RLock()
	dc := v.control
	v.mu.RUnlock()
	if dc == nil || dc.ReadyState() != webrtc.DataChannelStateOpen {
		return
	}
	if err := dc.SendText(msg); err != nil {
		log.Printf("[viewer:%s] could not send caption: %v", v.id, err)
	}
}

func (t *captionTap) stopFFmpeg() {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.ffmpeg != nil && t.ffmpeg.Process != nil {
		_ = t.ffmpeg.Process.Kill()
		t.ffmpeg = nil
	}
	if t.udpConn != nil {
		_ = t.udpConn.Close()
		t.udpConn = nil
	}
	if t.sdpPath != "" {
		_ = os.Remove(t.sdpPath)
		t.sdpPath = ""
	}
	t.enoughAudio = false
}

//

// captionTranscriber turns one WAV window into text.
func (t *engineTranscriber) Transcribe(wav []byte, t0 float64) (captionChunk, error) {
	if err := t.engine.ensure(); err != nil {
		return captionChunk{}, err
	}
	return t.transcribeWindow(wav, t0)
}

// newCaptionTap wires a tap; it subscribes and spawns ffmpeg lazily.
func newCaptionTap(ms *MediaSwitch, sourceID, recording string, tr captionTranscriber, cfg captionConfig) *captionTap {
	ctx, cancel := context.WithCancel(context.Background())
	return &captionTap{
		ms:         ms,
		sourceID:   sourceID,
		recording:  recording,
		liveID:     "live-" + shortID(sourceID),
		transcribe: tr,
		window:     int(cfg.WindowSecs * captionPCMBytesPerSec),
		shift:      int(cfg.ShiftSecs * captionPCMBytesPerSec),
		ctx:        ctx,
		cancel:     cancel,
	}
}

func shortID(id string) string {
	if i := strings.Index(id, "-"); i > 0 {
		return id[:8]
	}
	if len(id) > 8 {
		return id[:8]
	}
	return id
}

// onPacket fans audio RTP packets to the ffmpeg child over UDP loopback.
// Video is ignored. The ffmpeg child starts on the first audio packet, when
// the payload type is known.
func (t *captionTap) onPacket(kind string, pkt *rtp.Packet) {
	if kind == "video" || t.ctx.Err() != nil {
		return
	}
	t.mu.Lock()
	ready := t.enoughAudio
	t.mu.Unlock()
	if !ready {
		if err := t.startFFmpeg(pkt.PayloadType); err != nil {
			log.Printf("[live-captions:%s] ffmpeg unavailable, tap idle: %v", t.sourceID, err)
			return
		}
		t.mu.Lock()
		t.enoughAudio = true
		t.mu.Unlock()
	}
	raw, err := pkt.Marshal()
	if err != nil {
		return
	}
	t.udpWrite(raw)
}

// buildCaptionSDP describes the loopback Opus stream for ffmpeg.
func buildCaptionSDP(port int, payloadType uint8) string {
	return fmt.Sprintf(`v=0
o=- 0 0 IN IP4 127.0.0.1
s=live-captions
c=IN IP4 127.0.0.1
t=0 0
m=audio %d RTP/AVP %d
a=rtpmap:%d opus/48000/2
`, port, payloadType, payloadType)
}

// pickLoopbackPort binds :0 to discover a free loopback UDP port.
func pickLoopbackPort() (int, error) {
	pc, err := net.ListenPacket("udp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer pc.Close()
	return pc.LocalAddr().(*net.UDPAddr).Port, nil
}

// startFFmpeg spawns the Opus->PCM decoder for this tap.
func (t *captionTap) startFFmpeg(payloadType uint8) error {
	port, err := pickLoopbackPort()
	if err != nil {
		return err
	}
	sdp, err := os.CreateTemp("", "cap-*.sdp")
	if err != nil {
		return err
	}
	if _, err := sdp.WriteString(buildCaptionSDP(port, payloadType)); err != nil {
		_ = os.Remove(sdp.Name())
		return err
	}
	_ = sdp.Close()
	udp, err := net.DialUDP("udp", nil, &net.UDPAddr{IP: net.ParseIP("127.0.0.1"), Port: port})
	if err != nil {
		_ = os.Remove(sdp.Name())
		return err
	}
	cmd := exec.CommandContext(t.ctx, "ffmpeg",
		"-hide_banner", "-loglevel", "error",
		"-protocol_whitelist", "file,rtp,udp",
		"-f", "sdp", "-i", sdp.Name(),
		"-ar", "16000", "-ac", "1", "-f", "s16le", "pipe:1")
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		udp.Close()
		_ = os.Remove(sdp.Name())
		return err
	}
	cmd.Stderr = log.Writer()
	if err := cmd.Start(); err != nil {
		udp.Close()
		_ = os.Remove(sdp.Name())
		return err
	}
	t.mu.Lock()
	t.ffmpeg = cmd
	t.ffmpegPort = port
	t.sdpPath = sdp.Name()
	t.udpConn = udp
	t.mu.Unlock()
	// Give ffmpeg a beat to bind before packets arrive; loss here only
	// trims the stream head, which the first window absorbs anyway.
	time.Sleep(300 * time.Millisecond)
	go t.pcmLoop(stdout)
	if t.recording != "" {
		t.openFinalsFile()
	}
	log.Printf("[live-captions:%s] tap up (opus pt=%d, ffmpeg -> 16k mono)", t.sourceID, payloadType)
	return nil
}

//
