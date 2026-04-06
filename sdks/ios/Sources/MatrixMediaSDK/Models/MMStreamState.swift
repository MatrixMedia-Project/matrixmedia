import Foundation

/// State of an active ``MMStream`` connection.
public enum MMStreamState: Sendable, Equatable {
    /// Connecting to the SFU.
    case connecting

    /// Connected and streaming.
    case connected

    /// Temporarily disconnected; reconnecting via ``ReconnectPolicy``.
    case reconnecting(attempt: Int)

    /// Disconnected. Terminal state.
    case disconnected(reason: MMDisconnectReason)
}

/// State of the ``MMClient`` connection to mm-core.
public enum MMConnectionState: Sendable, Equatable {
    /// Not connected; ``authenticate()`` has not been called.
    case disconnected

    /// Authentication in progress.
    case authenticating

    /// Authenticated and ready to create/join streams.
    case connected

    /// Connection lost; SDK is attempting to reconnect.
    case reconnecting
}

// MARK: - Equatable conformance for MMDisconnectReason

extension MMDisconnectReason: Equatable {}
