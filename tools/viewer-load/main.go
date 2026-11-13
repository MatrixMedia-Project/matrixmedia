// Command viewer-load spawns N real share-link viewers against mm-core +
// mm-switch, the same way the web viewer does: POST public-join for switch
// credentials, then a plain WebRTC offer/answer to the switch with two
// recvonly transceivers and the mm-control data channel. Every bot is a
// REAL switch viewer — it shows up in the switch census the fleet planner
// reads, receives real RTP, and exercises the exact path under test.
//
//	for the fan-out threshold proof. It never publishes.
//
// Usage:
//
//	go run . -stream <stream-id> -grant <grant-id> -n 90 -ramp 10 -hold 10m
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/pion/webrtc/v4"
)

var iceServers = []webrtc.ICEServer{
	{URLs: []string{"stun:stun.l.google.com:19302"}},
	{URLs: []string{"turn:steegler.com:3478"}, Username: "mm", Credential: "mm_turn_secret_prod"},
	{URLs: []string{"turn:steegler.com:3478?transport=tcp"}, Username: "mm", Credential: "mm_turn_secret_prod"},
}

type joinResponse struct {
	SwitchURL         string `json:"switch_url"`
	SwitchSourceID    string `json:"switch_source_id"`
	SwitchViewerID    string `json:"switch_viewer_id"`
	SwitchViewerToken string `json:"switch_viewer_token"`
}

type offerRequest struct {
	ID          string                    `json:"id"`
	SourceID    string                    `json:"source_id"`
	Offer       webrtc.SessionDescription `json:"offer"`
	GrantID     string                    `json:"grant_id,omitempty"`
	ViewerToken string                    `json:"viewer_token,omitempty"`
}

type offerResponse struct {
	ID     string                    `json:"id"`
	Answer webrtc.SessionDescription `json:"answer"`
}

var (
	connected  atomic.Int64
	packets    atomic.Uint64
	joinErrors atomic.Int64
)

func main() {
	base := flag.String("base", "https://matrix.steegler.com", "public base URL of the deployment")
	stream := flag.String("stream", "", "stream id (required)")
	grant := flag.String("grant", "", "share grant id (required)")
	n := flag.Int("n", 10, "number of concurrent viewers")
	ramp := flag.Float64("ramp", 5, "viewers joining per second")
	hold := flag.Duration("hold", 10*time.Minute, "how long to hold viewers before closing")
	flag.Parse()
	if *stream == "" || *grant == "" {
		log.Fatal("-stream and -grant are required")
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	var wg sync.WaitGroup
	interval := time.Duration(float64(time.Second) / *ramp)
	for i := 0; i < *n; i++ {
		select {
		case <-ctx.Done():
			wg.Wait()
			return
		case <-time.After(interval):
		}
		wg.Add(1)
		go func(idx int) {
			defer wg.Done()
			if err := runViewer(ctx, *base, *stream, *grant, idx); err != nil {
				joinErrors.Add(1)
				log.Printf("viewer %d: %v", idx, err)
			}
		}(i)
	}

	// Aggregate reporter: the numbers the proof reads.
	done := make(chan struct{})
	go func() {
		t := time.NewTicker(10 * time.Second)
		defer t.Stop()
		for {
			select {
			case <-done:
				return
			case now := <-t.C:
				log.Printf(
					"[%s] connected=%d packets=%d join_errors=%d",
					now.Format("15:04:05"), connected.Load(), packets.Load(), joinErrors.Load(),
				)
			}
		}
	}()

	select {
	case <-ctx.Done():
		log.Printf("signal received, closing viewers")
	case <-time.After(*hold):
		log.Printf("hold elapsed, closing viewers")
	}
	close(done)
	stop()
	wg.Wait()
	log.Printf("final: connected=%d packets=%d join_errors=%d", connected.Load(), packets.Load(), joinErrors.Load())
}

func runViewer(ctx context.Context, base, streamID, grantID string, _ int) error {
	join, err := publicJoin(ctx, base, streamID, grantID)
	if err != nil {
		return err
	}
	if join.SwitchURL == "" || join.SwitchViewerID == "" || join.SwitchSourceID == "" {
		return fmt.Errorf("join response carries no switch credentials (switch_url=%q)", join.SwitchURL)
	}

	pc, err := webrtc.NewPeerConnection(webrtc.Configuration{ICEServers: iceServers})
	if err != nil {
		//nolint:wrapcheck
		return err
	}
	defer pc.Close() //nolint:errcheck

	pc.OnTrack(func(track *webrtc.TrackRemote, _ *webrtc.RTPReceiver) {
		buf := make([]byte, 1500)
		for {
			if _, _, err := track.Read(buf); err != nil {
				return
			}
			packets.Add(1)
		}
	})
	connectedOnce := make(chan struct{}, 1)
	pc.OnConnectionStateChange(func(s webrtc.PeerConnectionState) {
		if s == webrtc.PeerConnectionStateConnected {
			select {
			case connectedOnce <- struct{}{}:
			default:
			}
		}
	})

	// Control channel BEFORE the offer — mm-switch parses the first
	// data-channel m-line as control.
	if _, err = pc.CreateDataChannel("mm-control", nil); err != nil {
		//nolint:wrapcheck
		return err
	}
	// Pinned m-line order: audio, then video.
	if _, err = pc.AddTransceiverFromKind(webrtc.RTPCodecTypeAudio, webrtc.RTPTransceiverInit{Direction: webrtc.RTPTransceiverDirectionRecvonly}); err != nil {
		//nolint:wrapcheck
		return err
	}
	if _, err = pc.AddTransceiverFromKind(webrtc.RTPCodecTypeVideo, webrtc.RTPTransceiverInit{Direction: webrtc.RTPTransceiverDirectionRecvonly}); err != nil {
		//nolint:wrapcheck
		return err
	}

	offer, err := pc.CreateOffer(nil)
	if err != nil {
		//nolint:wrapcheck
		return err
	}
	gathered := webrtc.GatheringCompletePromise(pc)
	if err = pc.SetLocalDescription(offer); err != nil {
		//nolint:wrapcheck
		return err
	}
	<-gathered
	local := pc.LocalDescription()
	if local == nil {
		return fmt.Errorf("local description vanished after gather")
	}

	// The fleet viewer proxy admits anonymous share-link viewers from the
	// body's grant credential (the same shape the web viewer posts); the
	// direct switch path authenticates from the Authorization header both
	// carry. GrantID is threaded from the -grant flag through publicJoin's
	// caller.
	answer, err := postOffer(ctx, join, grantID, offerRequest{
		ID:       join.SwitchViewerID,
		SourceID: join.SwitchSourceID,
		Offer:    *local,
	})
	if err != nil {
		return err
	}
	if err = pc.SetRemoteDescription(answer); err != nil {
		//nolint:wrapcheck
		return err
	}

	select {
	case <-connectedOnce:
		connected.Add(1)
		defer connected.Add(-1)
	case <-ctx.Done():
		return nil
	case <-time.After(20 * time.Second):
		return fmt.Errorf("peer connection did not reach connected")
	}

	<-ctx.Done()
	return nil
}

func publicJoin(ctx context.Context, base, streamID, grantID string) (*joinResponse, error) {
	url := fmt.Sprintf("%s/_mm/client/v1/streams/%s/public-join", base, streamID)
	body, _ := json.Marshal(map[string]string{"grant_id": grantID})
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		return nil, err //nolint:wrapcheck
	}
	req.Header.Set("Content-Type", "application/json")
	client := &http.Client{Timeout: 20 * time.Second}
	res, err := client.Do(req)
	if err != nil {
		return nil, err //nolint:wrapcheck
	}
	defer res.Body.Close() //nolint:errcheck
	if res.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("public-join: HTTP %d", res.StatusCode)
	}
	var join joinResponse
	if err = json.NewDecoder(res.Body).Decode(&join); err != nil {
		return nil, fmt.Errorf("public-join: decode: %w", err)
	}
	return &join, nil
}

func postOffer(ctx context.Context, join *joinResponse, grantID string, offer offerRequest) (webrtc.SessionDescription, error) {
	var zero webrtc.SessionDescription
	offer.GrantID = grantID
	offer.ViewerToken = join.SwitchViewerToken
	body, _ := json.Marshal(offer)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, join.SwitchURL+"/api/viewers/offer", bytes.NewReader(body))
	if err != nil {
		return zero, err //nolint:wrapcheck
	}
	req.Header.Set("Content-Type", "application/json")
	if join.SwitchViewerToken != "" {
		req.Header.Set("Authorization", "Bearer "+join.SwitchViewerToken)
	}
	client := &http.Client{Timeout: 20 * time.Second}
	res, err := client.Do(req)
	if err != nil {
		return zero, err //nolint:wrapcheck
	}
	defer res.Body.Close() //nolint:errcheck
	if res.StatusCode != http.StatusOK {
		return zero, fmt.Errorf("switch offer: HTTP %d", res.StatusCode)
	}
	var resp offerResponse
	if err = json.NewDecoder(res.Body).Decode(&resp); err != nil {
		return zero, fmt.Errorf("switch offer: decode: %w", err)
	}
	return resp.Answer, nil
}
