import SwiftUI
#if canImport(AVKit)
import AVKit
#endif
#if canImport(AVFoundation)
import AVFoundation
#endif

/// SwiftUI view that plays back a recorded stream.
///
/// Uses AVPlayer for HLS/MP4/Ogg audio and video. The player streams directly
/// from the recording's `playbackUrl` (typically a CDN URL) without downloading
/// the entire file.
///
/// ## Usage
///
/// ```swift
/// let recordings = try await client.listRoomRecordings(roomID: "!abc:example.org")
/// if let first = recordings.first {
///     MMRecordingPlayer(recording: first)
/// }
/// ```
public struct MMRecordingPlayer: View {
    let recording: MMRecording

    #if canImport(AVKit)
    @State private var player: AVPlayer?
    #endif

    public init(recording: MMRecording) {
        self.recording = recording
    }

    public var body: some View {
        VStack(spacing: 8) {
            #if canImport(AVKit)
            if let url = recording.playbackUrl.flatMap(URL.init(string:)) {
                if recording.mediaType == .video || recording.mediaType == .screenShare {
                    VideoPlayer(player: player)
                        .aspectRatio(16.0 / 9.0, contentMode: .fit)
                        .onAppear {
                            let p = AVPlayer(url: url)
                            self.player = p
                            p.play()
                        }
                        .onDisappear {
                            player?.pause()
                        }
                } else {
                    AudioPlaybackView(
                        url: url,
                        title: recording.title,
                        duration: recording.durationMs
                    )
                }
            } else {
                unavailableView
            }
            #else
            unavailableView
            #endif

            if let title = recording.title {
                Text(title)
                    .font(.headline)
            }
            if let duration = recording.durationMs {
                Text(formatDuration(duration))
                    .font(.caption)
                    .foregroundColor(.secondary)
            }
        }
    }

    private var unavailableView: some View {
        Text("Recording not available")
            .foregroundColor(.secondary)
    }

    private func formatDuration(_ ms: Int64) -> String {
        let total = ms / 1000
        let h = total / 3600
        let m = (total % 3600) / 60
        let s = total % 60
        if h > 0 {
            return "\(h):\(String(format: "%02d", m)):\(String(format: "%02d", s))"
        }
        return "\(m):\(String(format: "%02d", s))"
    }
}

#if canImport(AVKit)
/// Simple audio playback view with a play/pause control.
private struct AudioPlaybackView: View {
    let url: URL
    let title: String?
    let duration: Int64?

    @State private var player: AVPlayer?
    @State private var isPlaying = false

    var body: some View {
        HStack(spacing: 12) {
            Button(action: togglePlay) {
                Image(systemName: isPlaying ? "pause.circle.fill" : "play.circle.fill")
                    .font(.system(size: 44))
            }
            .buttonStyle(.plain)

            VStack(alignment: .leading, spacing: 2) {
                Text(title ?? "Recording")
                    .font(.subheadline)
                    .lineLimit(1)
                if let duration = duration {
                    Text(formatDuration(duration))
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
            }
            Spacer()
        }
        .padding(.horizontal)
        .onAppear {
            player = AVPlayer(url: url)
        }
        .onDisappear {
            player?.pause()
            isPlaying = false
        }
    }

    private func togglePlay() {
        guard let player = player else { return }
        if isPlaying {
            player.pause()
        } else {
            player.play()
        }
        isPlaying.toggle()
    }

    private func formatDuration(_ ms: Int64) -> String {
        let total = ms / 1000
        let m = total / 60
        let s = total % 60
        return "\(m):\(String(format: "%02d", s))"
    }
}
#endif
