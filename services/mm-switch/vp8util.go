package main

// IsVP8Keyframe checks if raw VP8 frame data is a keyframe.
// First byte bit 0 == 0 means keyframe.
func IsVP8KeyframeRaw(data []byte) bool {
	return len(data) > 0 && (data[0]&0x01) == 0
}

// IsVP8StartOfPartition checks if an RTP payload starts a new VP8 partition.
// S bit (bit 4 of first byte) = 1 means start of partition.
func IsVP8StartOfPartition(payload []byte) bool {
	if len(payload) < 1 {
		return false
	}
	return (payload[0] & 0x10) != 0
}

// IsVP8Keyframe checks if an RTP VP8 payload is the start of a keyframe.
func IsVP8Keyframe(payload []byte) bool {
	if !IsVP8StartOfPartition(payload) {
		return false
	}
	data := ExtractVP8Payload(payload)
	if data == nil {
		return false
	}
	return (data[0] & 0x01) == 0
}

// ExtractVP8Payload strips the VP8 RTP payload descriptor (RFC 7741)
// and returns the raw VP8 data. Returns nil if payload is too short.
func ExtractVP8Payload(payload []byte) []byte {
	if len(payload) < 1 {
		return nil
	}

	idx := 0
	firstByte := payload[idx]
	idx++

	hasExtension := (firstByte & 0x80) != 0 // X bit

	if hasExtension && idx < len(payload) {
		extByte := payload[idx]
		idx++
		if (extByte & 0x80) != 0 { // I bit (PictureID)
			if idx < len(payload) {
				if (payload[idx] & 0x80) != 0 {
					idx += 2 // 16-bit PictureID
				} else {
					idx++ // 8-bit PictureID
				}
			}
		}
		if (extByte & 0x40) != 0 { // L bit (TL0PICIDX)
			idx++
		}
		if (extByte&0x20) != 0 || (extByte&0x10) != 0 { // T or K bit
			idx++
		}
	}

	if idx >= len(payload) {
		return nil
	}

	return payload[idx:]
}
