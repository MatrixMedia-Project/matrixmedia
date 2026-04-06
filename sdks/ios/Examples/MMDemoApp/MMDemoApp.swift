import SwiftUI

/// Minimal demo app showing MatrixMediaSDK integration.
///
/// Uses hardcoded dev config (no Matrix SDK dependency) to demonstrate:
/// - Client initialization with token provider
/// - Joining a stream
/// - Audio visualization
/// - Leaving a stream
@main
struct MMDemoApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}
