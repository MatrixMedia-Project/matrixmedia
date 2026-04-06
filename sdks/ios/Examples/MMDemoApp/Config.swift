import Foundation

/// Development configuration for the demo app.
///
/// In a real app these values come from the Matrix room state event
/// `com.matrixmedia.stream` field `mm_server_url`, or from user configuration.
///
/// For local development, use the host machine's LAN IP (not localhost)
/// because the iOS Simulator cannot reach localhost on the host
/// (PTT Knowledge Base lesson #11).
enum Config {
    /// mm-core server URL.
    ///
    /// Change this to your dev server's LAN IP.
    /// Example: `http://10.0.0.105:6167`
    static let serverURL = URL(string: "http://10.0.0.105:6167")!

    /// Dev OpenID token for testing.
    ///
    /// In production, the host app's Matrix SDK provides this via
    /// `MatrixClient.getOpenIDToken()`.
    static let devAccessToken = "dev-openid-token-for-testing"

    /// Dev Matrix server name.
    static let devMatrixServerName = "localhost"

    /// Default room ID for quick testing.
    static let defaultRoomID = "!test:localhost"
}
