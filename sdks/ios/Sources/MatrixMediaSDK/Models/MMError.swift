import Foundation

/// Errors emitted by the MatrixMedia SDK.
///
/// Maps server error codes (``MM_*``) to strongly-typed Swift cases.
/// Network and decoding errors are wrapped in ``networkError`` and ``decodingError``.
public enum MMError: Error, LocalizedError, Sendable {
    // MARK: - Authentication
    case notAuthenticated
    case tokenExpired
    case tokenRefreshFailed(underlying: Error)

    // MARK: - Stream
    case streamNotFound
    case streamFull
    case streamEnded
    case streamAlreadyActive
    case notAuthorized

    // MARK: - Device
    case microphonePermissionDenied

    // MARK: - Network
    case networkError(underlying: Error)
    case decodingError(underlying: Error)

    // MARK: - Server
    case serverError(code: String, message: String)
    case sfuUnavailable
    case rateLimited(retryAfterMs: Int?)
    case featureDisabled

    // MARK: - Client
    case invalidServerURL
    case invalidResponse

    public var errorDescription: String? {
        switch self {
        case .notAuthenticated:
            return "Not authenticated. Call authenticate() before making requests."
        case .tokenExpired:
            return "Session token has expired. Re-authenticate."
        case .tokenRefreshFailed(let underlying):
            return "Token refresh failed: \(underlying.localizedDescription)"
        case .streamNotFound:
            return "The requested stream does not exist or has ended."
        case .streamFull:
            return "The stream has reached its maximum participant limit."
        case .streamEnded:
            return "The stream has already ended."
        case .streamAlreadyActive:
            return "A stream is already active in this room."
        case .notAuthorized:
            return "You do not have permission to perform this action."
        case .microphonePermissionDenied:
            return "Microphone permission is required to host a stream."
        case .networkError(let underlying):
            return "Network error: \(underlying.localizedDescription)"
        case .decodingError(let underlying):
            return "Failed to decode server response: \(underlying.localizedDescription)"
        case .serverError(let code, let message):
            return "Server error [\(code)]: \(message)"
        case .sfuUnavailable:
            return "The streaming server (SFU) is temporarily unavailable."
        case .rateLimited(let retryAfterMs):
            if let ms = retryAfterMs {
                return "Rate limited. Retry after \(ms)ms."
            }
            return "Rate limited. Try again later."
        case .featureDisabled:
            return "This feature is currently disabled on the server."
        case .invalidServerURL:
            return "Invalid server URL."
        case .invalidResponse:
            return "Received an invalid response from the server."
        }
    }

    // MARK: - Server Error Code Mapping

    /// Maps an ``MMErrorResponse`` from the server to a typed ``MMError``.
    static func from(serverResponse: MMErrorResponse) -> MMError {
        switch serverResponse.error {
        case "MM_NOT_FOUND":
            return .streamNotFound
        case "MM_STREAM_ENDED":
            return .streamEnded
        case "MM_STREAM_ACTIVE":
            return .streamAlreadyActive
        case "MM_ROOM_FULL":
            return .streamFull
        case "MM_FORBIDDEN":
            return .notAuthorized
        case "MM_SFU_UNAVAILABLE":
            return .sfuUnavailable
        case "MM_RATE_LIMITED":
            return .rateLimited(retryAfterMs: serverResponse.retryAfterMs)
        case "MM_FEATURE_DISABLED":
            return .featureDisabled
        default:
            return .serverError(code: serverResponse.error, message: serverResponse.message)
        }
    }
}
