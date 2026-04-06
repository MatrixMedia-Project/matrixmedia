import SwiftUI

/// A SwiftUI view that visualizes audio from an ``MMStream``.
///
/// Binds to the stream's ``MMStream/audioLevel`` property and renders an animated
/// visualizer using ``AudioVisualizerView``.
///
/// ## Usage
/// ```swift
/// MMAudioRenderer(stream: stream, style: .bars)
///     .frame(height: 120)
/// ```
///
/// Supports three styles: ``.bars`` (default), ``.waveform``, and ``.minimal``.
/// When the user has enabled Reduce Motion in accessibility settings, animations
/// are disabled and a static level indicator is shown instead.
public struct MMAudioRenderer: View {
    @ObservedObject var stream: MMStream
    let style: MMAudioRendererStyle

    /// Creates a new audio renderer bound to a stream.
    ///
    /// - Parameters:
    ///   - stream: The ``MMStream`` providing audio level updates.
    ///   - style: Visual style. Defaults to ``.bars``.
    public init(stream: MMStream, style: MMAudioRendererStyle = .bars) {
        self.stream = stream
        self.style = style
    }

    public var body: some View {
        AudioVisualizerView(audioLevel: stream.audioLevel, style: style)
            .accessibilityLabel("Audio stream visualization")
            .accessibilityValue(audioLevelDescription)
    }

    private var audioLevelDescription: String {
        let percentage = Int(stream.audioLevel * 100)
        return "Audio level \(percentage) percent"
    }
}

/// Visual style for the ``MMAudioRenderer``.
public enum MMAudioRendererStyle: Sendable {
    /// Vertical frequency bars.
    case bars

    /// Continuous waveform.
    case waveform

    /// Minimal dot pulsing indicator.
    case minimal
}
