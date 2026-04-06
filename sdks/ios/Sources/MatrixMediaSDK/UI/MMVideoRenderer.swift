import SwiftUI

public struct MMVideoRenderer: View {
    @ObservedObject var stream: MMStream
    let fit: MMVideoFit

    public init(stream: MMStream, fit: MMVideoFit = .contain) {
        self.stream = stream
        self.fit = fit
    }

    public var body: some View {
        GeometryReader { geo in
            if stream.hasVideo {
                // Placeholder for LiveKit SwiftUIVideoView integration
                ZStack {
                    Rectangle().fill(Color.black)
                    Image(systemName: "video.fill")
                        .font(.largeTitle)
                        .foregroundColor(.white.opacity(0.5))
                    Text(stream.isScreenShare ? "Screen Share" : "Camera")
                        .foregroundColor(.white.opacity(0.7))
                        .font(.caption)
                        .padding(.top, 40)
                }
                .aspectRatio(16/9, contentMode: fit == .contain ? .fit : .fill)
                .cornerRadius(8)
            } else {
                MMAudioRenderer(stream: stream)
            }
        }
    }
}

public enum MMVideoFit: Sendable {
    case contain
    case cover
    case fill
}
