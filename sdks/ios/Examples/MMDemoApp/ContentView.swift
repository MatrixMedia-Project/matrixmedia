import SwiftUI
import MatrixMediaSDK

/// Main view: login and room selection.
struct ContentView: View {
    @StateObject private var client = MMClient(
        serverURL: Config.serverURL,
        tokenProvider: {
            // In a real app, this would call your Matrix SDK to get a fresh OpenID token.
            // NEVER cache this token. Return a fresh one each time.
            MMOpenIDToken(
                accessToken: Config.devAccessToken,
                tokenType: "Bearer",
                matrixServerName: Config.devMatrixServerName,
                expiresInMs: 3600000
            )
        }
    )

    @State private var roomID: String = Config.defaultRoomID
    @State private var isAuthenticated = false
    @State private var errorMessage: String?
    @State private var activeStream: MMStream?

    var body: some View {
        NavigationStack {
            VStack(spacing: 24) {
                // Status indicator
                HStack {
                    Circle()
                        .fill(statusColor)
                        .frame(width: 10, height: 10)
                    Text(statusText)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                if !isAuthenticated {
                    authSection
                } else if let stream = activeStream {
                    StreamView(stream: stream, onLeave: {
                        activeStream = nil
                    })
                } else {
                    streamSection
                    NavigationLink(destination: LightningView(client: client)) {
                        Label("Lightning settings & tips", systemImage: "bolt.fill")
                            .padding(.vertical, 6)
                    }
                    .buttonStyle(.bordered)
                }

                if let error = errorMessage {
                    Text(error)
                        .font(.caption)
                        .foregroundStyle(.red)
                        .multilineTextAlignment(.center)
                        .padding(.horizontal)
                }

                Spacer()
            }
            .padding()
            .navigationTitle("MatrixMedia Demo")
        }
    }

    // MARK: - Auth Section

    private var authSection: some View {
        VStack(spacing: 16) {
            Text("Connect to mm-core")
                .font(.headline)

            Text("Server: \(Config.serverURL.absoluteString)")
                .font(.caption)
                .foregroundStyle(.secondary)

            Button("Authenticate") {
                Task {
                    await authenticate()
                }
            }
            .buttonStyle(.borderedProminent)
        }
    }

    // MARK: - Stream Section

    private var streamSection: some View {
        VStack(spacing: 16) {
            Text("Join a Stream")
                .font(.headline)

            TextField("Room ID", text: $roomID)
                .textFieldStyle(.roundedBorder)
                .autocorrectionDisabled()
                #if os(iOS)
                .textInputAutocapitalization(.never)
                #endif

            HStack(spacing: 16) {
                Button("Join as Viewer") {
                    Task { await joinStream() }
                }
                .buttonStyle(.borderedProminent)

                Button("Start as Host") {
                    Task { await startStream() }
                }
                .buttonStyle(.bordered)
            }

            Button("Disconnect") {
                Task {
                    await client.disconnect()
                    isAuthenticated = false
                }
            }
            .font(.caption)
            .foregroundStyle(.red)
        }
    }

    // MARK: - Status

    private var statusColor: Color {
        switch client.connectionState {
        case .connected: return .green
        case .authenticating, .reconnecting: return .yellow
        case .disconnected: return .red
        }
    }

    private var statusText: String {
        switch client.connectionState {
        case .connected: return "Connected"
        case .authenticating: return "Authenticating..."
        case .reconnecting: return "Reconnecting..."
        case .disconnected: return "Disconnected"
        }
    }

    // MARK: - Actions

    private func authenticate() async {
        errorMessage = nil
        do {
            let user = try await client.authenticate()
            isAuthenticated = true
            errorMessage = nil
            print("Authenticated as \(user.id)")
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func joinStream() async {
        errorMessage = nil
        do {
            let stream = try await client.joinStream(roomID: roomID)
            activeStream = stream
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func startStream() async {
        errorMessage = nil
        do {
            let stream = try await client.startStream(
                roomID: roomID,
                config: MMStreamConfig(title: "Demo Stream")
            )
            activeStream = stream
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}
