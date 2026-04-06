import SwiftUI

/// Canvas-based audio visualizer that draws bars, waveform, or minimal indicator
/// based on the current audio level.
///
/// Respects the ``AccessibilityReduceMotion`` environment value: when enabled,
/// shows a static level indicator instead of animated bars.
struct AudioVisualizerView: View {
    let audioLevel: Float
    let style: MMAudioRendererStyle

    @Environment(\.accessibilityReduceMotion) var reduceMotion

    /// Number of bars in the bar visualizer.
    private let barCount = 12

    /// Animated level that smoothly tracks the input audio level.
    @State private var animatedLevel: Float = 0

    var body: some View {
        TimelineView(.animation(minimumInterval: reduceMotion ? nil : 1.0 / 30.0)) { timeline in
            Canvas { context, size in
                switch style {
                case .bars:
                    drawBars(context: context, size: size)
                case .waveform:
                    drawWaveform(context: context, size: size)
                case .minimal:
                    drawMinimal(context: context, size: size)
                }
            }
        }
        .frame(minHeight: 40, idealHeight: 120, maxHeight: 200)
        .onChange(of: audioLevel) { _, newValue in
            if reduceMotion {
                animatedLevel = newValue
            } else {
                withAnimation(.easeOut(duration: 0.08)) {
                    animatedLevel = newValue
                }
            }
        }
    }

    // MARK: - Bar Visualizer

    private func drawBars(context: GraphicsContext, size: CGSize) {
        let barWidth = size.width / CGFloat(barCount * 2 - 1)
        let maxBarHeight = size.height * 0.9
        let level = CGFloat(animatedLevel)

        for i in 0..<barCount {
            // Create varied heights using a simple deterministic pattern
            let variance = abs(sin(Double(i) * 0.7 + Double(animatedLevel) * 3.0))
            let barHeight = max(4, maxBarHeight * level * CGFloat(0.3 + 0.7 * variance))

            let x = CGFloat(i * 2) * barWidth
            let y = size.height - barHeight

            let rect = CGRect(x: x, y: y, width: barWidth, height: barHeight)
            let roundedRect = RoundedRectangle(cornerRadius: barWidth / 2)
                .path(in: rect)

            // Color gradient based on level
            let color: Color
            if level < 0.3 {
                color = .green
            } else if level < 0.7 {
                color = .yellow
            } else {
                color = .red
            }

            context.fill(roundedRect, with: .color(color.opacity(0.5 + 0.5 * Double(level))))
        }
    }

    // MARK: - Waveform Visualizer

    private func drawWaveform(context: GraphicsContext, size: CGSize) {
        let level = CGFloat(animatedLevel)
        let midY = size.height / 2
        let amplitude = size.height * 0.4 * level
        let segments = 60

        var path = Path()
        path.move(to: CGPoint(x: 0, y: midY))

        for i in 0...segments {
            let x = size.width * CGFloat(i) / CGFloat(segments)
            let phase = Double(i) * 0.3 + Double(animatedLevel) * 8.0
            let y = midY + amplitude * CGFloat(sin(phase))
            path.addLine(to: CGPoint(x: x, y: y))
        }

        context.stroke(
            path,
            with: .color(.accentColor),
            lineWidth: 2.0
        )
    }

    // MARK: - Minimal Indicator

    private func drawMinimal(context: GraphicsContext, size: CGSize) {
        let level = CGFloat(animatedLevel)
        let center = CGPoint(x: size.width / 2, y: size.height / 2)
        let maxRadius = min(size.width, size.height) / 2 * 0.8
        let radius = max(8, maxRadius * (0.3 + 0.7 * level))

        let circle = Path(
            ellipseIn: CGRect(
                x: center.x - radius,
                y: center.y - radius,
                width: radius * 2,
                height: radius * 2
            )
        )

        let color: Color = level > 0.01 ? .accentColor : .gray
        context.fill(circle, with: .color(color.opacity(0.4 + 0.6 * Double(level))))

        // Inner dot
        let innerRadius: CGFloat = 6
        let innerCircle = Path(
            ellipseIn: CGRect(
                x: center.x - innerRadius,
                y: center.y - innerRadius,
                width: innerRadius * 2,
                height: innerRadius * 2
            )
        )
        context.fill(innerCircle, with: .color(color))
    }
}

// MARK: - Preview

#if DEBUG
struct AudioVisualizerView_Previews: PreviewProvider {
    static var previews: some View {
        VStack(spacing: 20) {
            AudioVisualizerView(audioLevel: 0.5, style: .bars)
                .frame(height: 120)
            AudioVisualizerView(audioLevel: 0.7, style: .waveform)
                .frame(height: 120)
            AudioVisualizerView(audioLevel: 0.3, style: .minimal)
                .frame(height: 120)
        }
        .padding()
    }
}
#endif
