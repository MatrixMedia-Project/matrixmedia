import XCTest
@testable import MatrixMediaSDK

/// Tests for the dual ISO8601 date decoder (PTT Knowledge Base bug #10 fix).
///
/// Go's `time.Time.MarshalJSON` emits fractional seconds (nanoseconds).
/// Rust's serde emits fractional seconds (microseconds).
/// Swift's `.iso8601` DateDecodingStrategy rejects both.
/// The `MMDateDecoder` handles all formats.
final class MMDateDecoderTests: XCTestCase {

    private var decoder: JSONDecoder!

    override func setUp() {
        super.setUp()
        decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .custom(MMDateDecoder.decode)
    }

    // MARK: - Wrapper for testing

    private struct DateWrapper: Codable {
        let date: Date
    }

    private func decodeDate(from json: String) throws -> Date {
        let data = json.data(using: .utf8)!
        return try decoder.decode(DateWrapper.self, from: data).date
    }

    // MARK: - Fractional Seconds (Go / Rust format)

    func testDecodesGoNanoseconds() throws {
        // Go emits: "2025-01-15T12:00:00.123456789Z"
        let date = try decodeDate(from: #"{"date": "2025-01-15T12:00:00.123456789Z"}"#)
        XCTAssertNotNil(date)

        let calendar = Calendar(identifier: .gregorian)
        let components = calendar.dateComponents(in: TimeZone(identifier: "UTC")!, from: date)
        XCTAssertEqual(components.year, 2025)
        XCTAssertEqual(components.month, 1)
        XCTAssertEqual(components.day, 15)
        XCTAssertEqual(components.hour, 12)
        XCTAssertEqual(components.minute, 0)
        XCTAssertEqual(components.second, 0)
    }

    func testDecodesRustMicroseconds() throws {
        // Rust serde emits: "2025-06-20T08:30:15.123456Z"
        let date = try decodeDate(from: #"{"date": "2025-06-20T08:30:15.123456Z"}"#)
        XCTAssertNotNil(date)

        let calendar = Calendar(identifier: .gregorian)
        let components = calendar.dateComponents(in: TimeZone(identifier: "UTC")!, from: date)
        XCTAssertEqual(components.year, 2025)
        XCTAssertEqual(components.month, 6)
        XCTAssertEqual(components.day, 20)
        XCTAssertEqual(components.hour, 8)
        XCTAssertEqual(components.minute, 30)
    }

    func testDecodesMilliseconds() throws {
        // Common format: "2025-03-01T00:00:00.500Z"
        let date = try decodeDate(from: #"{"date": "2025-03-01T00:00:00.500Z"}"#)
        XCTAssertNotNil(date)
    }

    // MARK: - Plain ISO8601 (no fractional seconds)

    func testDecodesPlainISO8601() throws {
        let date = try decodeDate(from: #"{"date": "2025-01-15T12:00:00Z"}"#)
        XCTAssertNotNil(date)

        let calendar = Calendar(identifier: .gregorian)
        let components = calendar.dateComponents(in: TimeZone(identifier: "UTC")!, from: date)
        XCTAssertEqual(components.year, 2025)
        XCTAssertEqual(components.month, 1)
        XCTAssertEqual(components.day, 15)
        XCTAssertEqual(components.hour, 12)
    }

    func testDecodesWithTimezoneOffset() throws {
        let date = try decodeDate(from: #"{"date": "2025-01-15T12:00:00+05:30"}"#)
        XCTAssertNotNil(date)
    }

    // MARK: - Unix Timestamp

    func testDecodesUnixTimestamp() throws {
        // Some APIs return timestamps as seconds since epoch
        let date = try decodeDate(from: #"{"date": 1705323600.0}"#)
        XCTAssertNotNil(date)
    }

    // MARK: - Invalid Dates

    func testRejectsInvalidDateString() {
        XCTAssertThrowsError(
            try decodeDate(from: #"{"date": "not-a-date"}"#)
        ) { error in
            guard case DecodingError.dataCorrupted = error else {
                XCTFail("Expected DecodingError.dataCorrupted, got \(error)")
                return
            }
        }
    }

    func testRejectsEmptyString() {
        XCTAssertThrowsError(
            try decodeDate(from: #"{"date": ""}"#)
        ) { error in
            guard case DecodingError.dataCorrupted = error else {
                XCTFail("Expected DecodingError.dataCorrupted, got \(error)")
                return
            }
        }
    }

    func testRejectsPartialDate() {
        XCTAssertThrowsError(
            try decodeDate(from: #"{"date": "2025-01-15"}"#)
        ) { error in
            guard case DecodingError.dataCorrupted = error else {
                XCTFail("Expected DecodingError.dataCorrupted, got \(error)")
                return
            }
        }
    }
}
