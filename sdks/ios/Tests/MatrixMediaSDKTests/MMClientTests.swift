import XCTest
@testable import MatrixMediaSDK

final class MMClientTests: XCTestCase {

    // MARK: - Server URL Validation

    func testInitWithHTTPSURL() {
        // HTTPS URLs should always be accepted.
        let url = URL(string: "https://mm.example.com")!
        let client = MMClient(serverURL: url) {
            throw MMError.notAuthenticated // Token provider not used in this test
        }
        XCTAssertNotNil(client)
    }

    func testInitialConnectionState() async {
        let url = URL(string: "https://mm.example.com")!
        let client = MMClient(serverURL: url) {
            throw MMError.notAuthenticated
        }
        let state = await client.connectionState
        XCTAssertEqual(state, .disconnected)
    }

    func testCurrentUserNilBeforeAuth() async {
        let url = URL(string: "https://mm.example.com")!
        let client = MMClient(serverURL: url) {
            throw MMError.notAuthenticated
        }
        let user = await client.currentUser
        XCTAssertNil(user)
    }

    // MARK: - Error Mapping

    func testErrorFromServerCode_NotFound() {
        let response = MMErrorResponse(error: "MM_NOT_FOUND", message: "Stream not found", retryAfterMs: nil)
        let error = MMError.from(serverResponse: response)
        switch error {
        case .streamNotFound:
            break // Expected
        default:
            XCTFail("Expected .streamNotFound, got \(error)")
        }
    }

    func testErrorFromServerCode_StreamEnded() {
        let response = MMErrorResponse(error: "MM_STREAM_ENDED", message: "Already ended", retryAfterMs: nil)
        let error = MMError.from(serverResponse: response)
        switch error {
        case .streamEnded:
            break
        default:
            XCTFail("Expected .streamEnded, got \(error)")
        }
    }

    func testErrorFromServerCode_RoomFull() {
        let response = MMErrorResponse(error: "MM_ROOM_FULL", message: "Room is full", retryAfterMs: nil)
        let error = MMError.from(serverResponse: response)
        switch error {
        case .streamFull:
            break
        default:
            XCTFail("Expected .streamFull, got \(error)")
        }
    }

    func testErrorFromServerCode_Forbidden() {
        let response = MMErrorResponse(error: "MM_FORBIDDEN", message: "Not allowed", retryAfterMs: nil)
        let error = MMError.from(serverResponse: response)
        switch error {
        case .notAuthorized:
            break
        default:
            XCTFail("Expected .notAuthorized, got \(error)")
        }
    }

    func testErrorFromServerCode_SFUUnavailable() {
        let response = MMErrorResponse(error: "MM_SFU_UNAVAILABLE", message: "SFU down", retryAfterMs: nil)
        let error = MMError.from(serverResponse: response)
        switch error {
        case .sfuUnavailable:
            break
        default:
            XCTFail("Expected .sfuUnavailable, got \(error)")
        }
    }

    func testErrorFromServerCode_RateLimited() {
        let response = MMErrorResponse(error: "MM_RATE_LIMITED", message: "Too fast", retryAfterMs: 5000)
        let error = MMError.from(serverResponse: response)
        switch error {
        case .rateLimited(let ms):
            XCTAssertEqual(ms, 5000)
        default:
            XCTFail("Expected .rateLimited, got \(error)")
        }
    }

    func testErrorFromServerCode_Unknown() {
        let response = MMErrorResponse(error: "MM_UNKNOWN_CODE", message: "Something weird", retryAfterMs: nil)
        let error = MMError.from(serverResponse: response)
        switch error {
        case .serverError(let code, let message):
            XCTAssertEqual(code, "MM_UNKNOWN_CODE")
            XCTAssertEqual(message, "Something weird")
        default:
            XCTFail("Expected .serverError, got \(error)")
        }
    }

    // MARK: - Error Descriptions

    func testErrorDescriptions() {
        let errors: [MMError] = [
            .notAuthenticated,
            .streamNotFound,
            .streamFull,
            .streamEnded,
            .notAuthorized,
            .microphonePermissionDenied,
            .invalidServerURL,
            .invalidResponse,
            .sfuUnavailable,
            .featureDisabled,
        ]

        for error in errors {
            XCTAssertNotNil(error.errorDescription, "Missing description for \(error)")
            XCTAssertFalse(error.errorDescription!.isEmpty, "Empty description for \(error)")
        }
    }
}
