import Foundation

/// Dual ISO8601 date decoder that handles both plain and fractional-second formats.
///
/// **PTT Knowledge Base bug #10:** Go's `time.Time.MarshalJSON` and Rust's serde both
/// emit fractional seconds (e.g., `"2025-01-15T12:00:00.123456789Z"`). Swift's built-in
/// `.iso8601` date decoding strategy rejects fractional seconds, causing silent decode
/// failures. This decoder tries fractional seconds first (most common from backend),
/// then falls back to plain ISO8601.
enum MMDateDecoder {

    // MARK: - Formatters

    /// ISO8601 with fractional seconds (handles Go nano, Rust micro, etc.)
    private static let fractionalFormatter: ISO8601DateFormatter = {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [
            .withInternetDateTime,
            .withFractionalSeconds,
        ]
        return formatter
    }()

    /// Plain ISO8601 without fractional seconds.
    private static let plainFormatter: ISO8601DateFormatter = {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        return formatter
    }()

    // MARK: - Decoder

    /// Custom `DateDecodingStrategy` function.
    ///
    /// Usage: `decoder.dateDecodingStrategy = .custom(MMDateDecoder.decode)`
    static func decode(_ decoder: Decoder) throws -> Date {
        let container = try decoder.singleValueContainer()

        // Try integer (Unix timestamp in seconds) first
        if let timestamp = try? container.decode(Double.self) {
            return Date(timeIntervalSince1970: timestamp)
        }

        // Try string-based formats
        let string = try container.decode(String.self)

        // Try fractional seconds first (most common from Go/Rust backends)
        if let date = fractionalFormatter.date(from: string) {
            return date
        }

        // Try plain ISO8601
        if let date = plainFormatter.date(from: string) {
            return date
        }

        throw DecodingError.dataCorruptedError(
            in: container,
            debugDescription: "Cannot decode date from '\(string)'. Expected ISO8601 format."
        )
    }
}
