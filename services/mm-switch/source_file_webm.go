package main

import (
	"bytes"
	"io"
	"log"
	"sort"
	"time"

	"github.com/at-wat/ebml-go"
	"github.com/at-wat/ebml-go/webm"
	"github.com/pion/rtp"
	"github.com/pion/rtp/codecs"
)

// Top-level WebM document wrapping the library's predefined types.
type webmDoc struct {
	Header  webm.EBMLHeader `ebml:"EBML"`
	Segment webm.Segment    `ebml:"Segment"`
}

type mediaBlock struct {
	trackType uint64 // 1=video, 2=audio
	timeMs    int64
	data      []byte
	keyframe  bool
}

// playWebM reads a WebM file (VP8 video + Opus audio), demuxes both tracks,
// and plays them back at the correct rate as RTP.
func (s *FileSource) playWebM() {
	rdr, closer, err := openSource(s.path)
	if err != nil {
		log.Printf("[file-source:%s] open %s failed: %v", s.id, s.path, err)
		time.Sleep(2 * time.Second)
		return
	}
	if closer != nil {
		defer closer.Close()
	}

	raw, err := io.ReadAll(rdr)
	if err != nil {
		log.Printf("[file-source:%s] read failed: %v", s.id, err)
		return
	}

	var doc webmDoc
	if err := ebml.Unmarshal(bytes.NewReader(raw), &doc); err != nil {
		log.Printf("[file-source:%s] webm parse failed: %v", s.id, err)
		return
	}

	// Identify tracks
	trackTypes := make(map[uint64]uint64) // trackNumber → trackType
	for _, e := range doc.Segment.Tracks.TrackEntry {
		trackTypes[e.TrackNumber] = e.TrackType
		log.Printf("[file-source:%s] track %d: type=%d codec=%s",
			s.id, e.TrackNumber, e.TrackType, e.CodecID)
	}

	// Timescale: ns per tick (default 1ms = 1000000ns)
	timescaleNs := doc.Segment.Info.TimecodeScale
	if timescaleNs == 0 {
		timescaleNs = 1000000
	}

	// Collect all blocks with absolute timestamps.
	// WebM SimpleBlocks can contain multiple frames via lacing.
	// For laced audio, each sub-frame covers 20ms (Opus default).
	// We must offset each sub-frame so they don't all share the same timestamp.
	var blocks []mediaBlock
	for _, cluster := range doc.Segment.Cluster {
		clusterMs := int64(cluster.Timecode * timescaleNs / 1_000_000)
		for _, sb := range cluster.SimpleBlock {
			absMs := clusterMs + int64(sb.Timecode)
			tt := trackTypes[sb.TrackNumber]
			for fi, frame := range sb.Data {
				frameCopy := make([]byte, len(frame))
				copy(frameCopy, frame)
				// For laced audio: offset each sub-frame by 20ms * index
				frameTimeMs := absMs
				if tt == 2 && len(sb.Data) > 1 {
					frameTimeMs = absMs + int64(fi)*20
				}
				blocks = append(blocks, mediaBlock{
					trackType: tt,
					timeMs:    frameTimeMs,
					data:      frameCopy,
					keyframe:  sb.Keyframe,
				})
			}
		}
	}

	sort.Slice(blocks, func(i, j int) bool { return blocks[i].timeMs < blocks[j].timeMs })
	log.Printf("[file-source:%s] webm loaded: %d blocks, timescale=%dns",
		s.id, len(blocks), timescaleNs)

	if len(blocks) == 0 {
		log.Printf("[file-source:%s] no blocks found", s.id)
		return
	}

	// Cache first substantial video keyframe for instant viewer start
	if len(s.cachedKF) == 0 {
		payloader := &codecs.VP8Payloader{EnablePictureID: true}
		for _, b := range blocks {
			if b.trackType == 1 && b.keyframe && len(b.data) > 1024 {
				rtpPayloads := payloader.Payload(1200, b.data)
				for j, payload := range rtpPayloads {
					pkt := &rtp.Packet{
						Header: rtp.Header{
							Version: 2, PayloadType: 96,
							SequenceNumber: uint16(j + 1),
							Timestamp: 0, Marker: j == len(rtpPayloads)-1, SSRC: 1,
						},
						Payload: payload,
					}
					s.cachedKF = append(s.cachedKF, pkt)
				}
				log.Printf("[file-source:%s] cached keyframe (%d pkts, %d bytes)",
					s.id, len(s.cachedKF), len(b.data))
				break
			}
		}
	}

	// Playback
	videoPayloader := &codecs.VP8Payloader{EnablePictureID: true}
	var videoSeq, audioSeq uint16
	startTime := time.Now()
	baseMs := blocks[0].timeMs
	videoCount, audioCount := 0, 0

	for _, b := range blocks {
		select {
		case <-s.stopCh:
			return
		default:
		}

		offsetMs := b.timeMs - baseMs
		dueAt := startTime.Add(time.Duration(offsetMs) * time.Millisecond)
		if delay := time.Until(dueAt); delay > 0 {
			select {
			case <-s.stopCh:
				return
			case <-time.After(delay):
			}
		}

		switch b.trackType {
		case 1: // Video (VP8)
			videoCount++
			rtpPayloads := videoPayloader.Payload(1200, b.data)
			for i, payload := range rtpPayloads {
				videoSeq++
				pkt := &rtp.Packet{
					Header: rtp.Header{
						Version: 2, PayloadType: 96,
						SequenceNumber: videoSeq,
						Timestamp:      uint32(offsetMs * 90),
						Marker:         i == len(rtpPayloads)-1,
						SSRC:           1,
					},
					Payload: payload,
				}
				s.mu.RLock()
				for _, h := range s.subscribers {
					h("video", pkt)
				}
				s.mu.RUnlock()
			}

		case 2: // Audio (Opus)
			audioCount++
			audioSeq++
			pkt := &rtp.Packet{
				Header: rtp.Header{
					Version: 2, PayloadType: 111,
					SequenceNumber: audioSeq,
					Timestamp:      uint32(offsetMs * 48),
					Marker:         true,
					SSRC:           2,
				},
				Payload: b.data,
			}
			s.mu.RLock()
			for _, h := range s.subscribers {
				h("audio", pkt)
			}
			s.mu.RUnlock()
		}

		if videoCount == 1 && b.trackType == 1 {
			log.Printf("[file-source:%s] first video (%d bytes, key=%v, t=%dms)", s.id, len(b.data), b.keyframe, offsetMs)
		}
		if audioCount <= 5 && b.trackType == 2 {
			tocByte := byte(0)
			if len(b.data) > 0 { tocByte = b.data[0] }
			// Opus TOC: config (bits 7-3), stereo (bit 2), frame count code (bits 1-0)
			config := tocByte >> 3
			stereo := (tocByte >> 2) & 1
			code := tocByte & 3
			log.Printf("[file-source:%s] audio #%d: t=%dms size=%d rtp_ts=%d toc=0x%02x config=%d stereo=%d code=%d wallclock=%v",
				s.id, audioCount, offsetMs, len(b.data), uint32(offsetMs*48),
				tocByte, config, stereo, code, time.Since(startTime).Round(time.Millisecond))
		}
		if (videoCount+audioCount)%500 == 0 {
			log.Printf("[file-source:%s] %d video + %d audio (t=%dms)",
				s.id, videoCount, audioCount, offsetMs)
		}
	}

	log.Printf("[file-source:%s] webm done (%d video, %d audio)", s.id, videoCount, audioCount)
}
