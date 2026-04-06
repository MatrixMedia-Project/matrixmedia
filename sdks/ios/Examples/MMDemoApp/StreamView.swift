import SwiftUI
import MatrixMediaSDK

/// Active stream view showing audio visualization, controls, and participant info.
struct StreamView: View {
    @ObservedObject var stream: MMStream
    let onLeave: () -> Void

    @State private var volume: Float = 1.0
    @State private var visualizerStyle: MMAudioRendererStyle = .bars

    var body: some View {
        VStack(spacing: 20) {
            // Stream status
            HStack {
                Circle()
                    .fill(streamStateColor)
                    .frame(width: 10, height: 10)
                Text(streamStateText)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Text("\(stream.viewerCount) viewers")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            // Audio visualizer
            MMAudioRenderer(stream: stream, style: visualizerStyle)
                .frame(height: 150)
                .background(Color(.systemGray6))
                .clipShape(RoundedRectangle(cornerRadius: 12))

            // Visualizer style picker
            Picker("Style", selection: $visualizerStyle) {
                Text("Bars").tag(MMAudioRendererStyle.bars)
                Text("Wave").tag(MMAudioRendererStyle.waveform)
                Text("Dot").tag(MMAudioRendererStyle.minimal)
            }
            .pickerStyle(.segmented)

            // Volume control
            HStack {
                Image(systemName: "speaker.fill")
                Slider(value: $volume, in: 0...1, step: 0.05)
                    .onChange(of: volume) { _, newValue in
                        stream.setVolume(newValue)
                    }
                Image(systemName: "speaker.wave.3.fill")
            }
            .foregroundStyle(.secondary)

            // Host info
            if let host = stream.host {
                HStack {
                    Image(systemName: "mic.fill")
                    Text(host.displayName ?? host.userID)
                        .font(.subheadline)
                    Spacer()
                }
                .foregroundStyle(.secondary)
            }

            Spacer()

            // Controls
            HStack(spacing: 24) {
                Button {
                    stream.setMuted(!stream.isMuted)
                } label: {
                    Image(systemName: stream.isMuted ? "mic.slash.fill" : "mic.fill")
                        .font(.title2)
                        .frame(width: 56, height: 56)
                        .background(stream.isMuted ? Color.red : Color.accentColor)
                        .foregroundStyle(.white)
                        .clipShape(Circle())
                }

                Button {
                    Task {
                        await stream.leave()
                        onLeave()
                    }
                } label: {
                    Image(systemName: "phone.down.fill")
                        .font(.title2)
                        .frame(width: 56, height: 56)
                        .background(Color.red)
                        .foregroundStyle(.white)
                        .clipShape(Circle())
                }
            }
        }
        .padding()
    }

    // MARK: - State Display

    private var streamStateColor: Color {
        switch stream.state {
        case .connected:
            return .green
        case .connecting, .reconnecting:
            return .yellow
        case .disconnected:
            return .red
        }
    }

    private var streamStateText: String {
        switch stream.state {
        case .connecting:
            return "Connecting..."
        case .connected:
            return "Live"
        case .reconnecting(let attempt):
            return "Reconnecting (attempt \(attempt))..."
        case .disconnected(let reason):
            switch reason {
            case .userInitiated:
                return "Disconnected"
            case .streamEnded:
                return "Stream ended"
            case .kicked:
                return "Removed from stream"
            case .networkError:
                return "Network error"
            case .serverError:
                return "Server error"
            case .tokenExpired:
                return "Session expired"
            case .unknown:
                return "Disconnected"
            }
        }
    }
}

// MARK: - MMAudioRendererStyle conformance for Picker

extension MMAudioRendererStyle: Hashable {}
