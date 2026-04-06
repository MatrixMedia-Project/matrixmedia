import Foundation

/// HTTP client for all communication with the mm-core REST API.
///
/// All authenticated requests go through ``TokenManager/getToken()`` to ensure
/// a valid session JWT is used. Tokens are never cached directly in this class.
///
/// Decoding uses ``MMDateDecoder`` to handle both ISO8601 and RFC3339 with
/// fractional seconds (PTT Knowledge Base bug #10).
final class MMAPIClient: @unchecked Sendable {

    // MARK: - Constants

    private enum APIPath {
        static let base = "/_mm/client/v1"
        static let authToken = "\(base)/auth/token"
        static let authRefresh = "\(base)/auth/token/refresh"
        static func stream(id: String) -> String { "\(base)/streams/\(id)" }
        static func streamJoin(id: String) -> String { "\(base)/streams/\(id)/join" }
        static func streamLeave(id: String) -> String { "\(base)/streams/\(id)/leave" }
        static func streamEnd(id: String) -> String { "\(base)/streams/\(id)/end" }
        static func streamParticipants(id: String) -> String { "\(base)/streams/\(id)/participants" }
        static func roomStreams(roomID: String) -> String { "\(base)/rooms/\(roomID)/streams" }
        static let createStream = "\(base)/streams"
        static func roomRecordings(roomID: String) -> String { "\(base)/rooms/\(roomID)/recordings" }
        static func recording(id: String) -> String { "\(base)/recordings/\(id)" }
    }

    // MARK: - Properties

    private let baseURL: URL
    private let tokenManager: TokenManager
    private let session: URLSession
    private let decoder: JSONDecoder

    // MARK: - Init

    init(baseURL: URL, tokenManager: TokenManager) {
        self.baseURL = baseURL
        self.tokenManager = tokenManager

        let config = URLSessionConfiguration.default
        config.timeoutIntervalForRequest = 15
        config.timeoutIntervalForResource = 30
        config.waitsForConnectivity = true
        self.session = URLSession(configuration: config)

        self.decoder = JSONDecoder()
        self.decoder.dateDecodingStrategy = .custom(MMDateDecoder.decode)
    }

    // MARK: - Auth API (unauthenticated)

    /// Exchange a Matrix OpenID token for an MM session JWT.
    func exchangeToken(openIDToken: MMOpenIDToken) async throws -> MMAuthToken {
        let body: [String: Any] = [
            "access_token": openIDToken.accessToken,
            "token_type": openIDToken.tokenType,
            "matrix_server_name": openIDToken.matrixServerName,
            "expires_in": openIDToken.expiresInMs,
        ]

        let data = try await post(path: APIPath.authToken, body: body, authenticated: false)

        struct TokenResponse: Codable {
            let token: String
            let refreshToken: String
            let expiresIn: Int
            let userId: String
            let displayName: String?

            enum CodingKeys: String, CodingKey {
                case token
                case refreshToken = "refresh_token"
                case expiresIn = "expires_in"
                case userId = "user_id"
                case displayName = "display_name"
            }
        }

        let response = try decoder.decode(TokenResponse.self, from: data)
        let expiresAt = Date().addingTimeInterval(TimeInterval(response.expiresIn))
        return MMAuthToken(
            sessionToken: response.token,
            refreshToken: response.refreshToken,
            expiresAt: expiresAt,
            userId: response.userId
        )
    }

    /// Refresh an expired session JWT using the refresh token.
    func refreshToken(refreshToken: String) async throws -> MMAuthToken {
        let body: [String: Any] = [
            "refresh_token": refreshToken,
        ]

        let data = try await post(path: APIPath.authRefresh, body: body, authenticated: false)

        struct RefreshResponse: Codable {
            let token: String
            let refreshToken: String
            let expiresIn: Int

            enum CodingKeys: String, CodingKey {
                case token
                case refreshToken = "refresh_token"
                case expiresIn = "expires_in"
            }
        }

        let response = try decoder.decode(RefreshResponse.self, from: data)
        let expiresAt = Date().addingTimeInterval(TimeInterval(response.expiresIn))
        return MMAuthToken(
            sessionToken: response.token,
            refreshToken: response.refreshToken,
            expiresAt: expiresAt,
            userId: "" // Preserved from existing session
        )
    }

    // MARK: - Stream API (authenticated)

    /// Fetch the active stream for a Matrix room.
    func getActiveStream(roomID: String) async throws -> MMStreamInfo {
        let data = try await get(path: APIPath.roomStreams(roomID: roomID))

        struct StreamListResponse: Codable {
            let streams: [MMStreamInfo]
        }

        let response = try decoder.decode(StreamListResponse.self, from: data)
        guard let stream = response.streams.first(where: { $0.status == .active }) else {
            throw MMError.streamNotFound
        }
        return stream
    }

    /// List all streams (active and ended) for a Matrix room.
    func listStreams(roomID: String) async throws -> [MMStreamInfo] {
        let data = try await get(path: APIPath.roomStreams(roomID: roomID))

        struct StreamListResponse: Codable {
            let streams: [MMStreamInfo]
        }

        let response = try decoder.decode(StreamListResponse.self, from: data)
        return response.streams
    }

    /// Get details for a specific stream.
    func getStream(streamID: String) async throws -> MMStreamInfo {
        let data = try await get(path: APIPath.stream(id: streamID))
        return try decoder.decode(MMStreamInfo.self, from: data)
    }

    /// Create a new stream (host only).
    func createStream(roomID: String, config: MMStreamConfig) async throws -> MMCreateStreamResponse {
        var body: [String: Any] = [
            "room_id": roomID,
            "media_type": config.mediaType.rawValue,
            "e2ee": config.e2ee,
        ]
        if let title = config.title {
            body["title"] = title
        }
        if let maxParticipants = config.maxParticipants {
            body["max_participants"] = maxParticipants
        }

        let data = try await post(path: APIPath.createStream, body: body)
        return try decoder.decode(MMCreateStreamResponse.self, from: data)
    }

    /// Join a stream as a viewer. Returns SFU connection details.
    func joinStream(streamID: String) async throws -> MMJoinResponse {
        let data = try await post(path: APIPath.streamJoin(id: streamID), body: [:])
        return try decoder.decode(MMJoinResponse.self, from: data)
    }

    /// Leave a stream.
    func leaveStream(streamID: String) async throws {
        _ = try await post(path: APIPath.streamLeave(id: streamID), body: [:])
    }

    /// End a stream (host only).
    func endStream(streamID: String) async throws {
        _ = try await post(path: APIPath.streamEnd(id: streamID), body: [:])
    }

    // MARK: - Recordings API (authenticated)

    /// Response envelope for listing recordings in a room.
    struct RecordingsResponse: Codable, Sendable {
        let recordings: [MMRecording]
    }

    /// List recordings for a Matrix room. Returns only recordings with status == ready.
    func listRoomRecordings(roomId: String, limit: Int) async throws -> RecordingsResponse {
        let queryItems = [
            URLQueryItem(name: "limit", value: String(limit)),
            URLQueryItem(name: "status", value: MMRecordingStatus.ready.rawValue),
        ]
        let data = try await getWithQuery(
            path: APIPath.roomRecordings(roomID: roomId),
            queryItems: queryItems
        )
        return try decoder.decode(RecordingsResponse.self, from: data)
    }

    /// Get details for a specific recording.
    func getRecording(recordingId: String) async throws -> MMRecording {
        let data = try await get(path: APIPath.recording(id: recordingId))
        return try decoder.decode(MMRecording.self, from: data)
    }

    /// Delete a recording (host only).
    func deleteRecording(recordingId: String) async throws {
        _ = try await delete(path: APIPath.recording(id: recordingId))
    }

    // MARK: - HTTP Primitives

    private func get(path: String) async throws -> Data {
        let url = baseURL.appendingPathComponent(path)
        var request = URLRequest(url: url)
        request.httpMethod = "GET"
        request.setValue("application/json", forHTTPHeaderField: "Accept")

        // Add auth header
        let token = try await tokenManager.getToken()
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")

        return try await execute(request)
    }

    private func getWithQuery(path: String, queryItems: [URLQueryItem]) async throws -> Data {
        let baseWithPath = baseURL.appendingPathComponent(path)
        var components = URLComponents(url: baseWithPath, resolvingAgainstBaseURL: false)
        components?.queryItems = queryItems
        guard let url = components?.url else {
            throw MMError.invalidResponse
        }

        var request = URLRequest(url: url)
        request.httpMethod = "GET"
        request.setValue("application/json", forHTTPHeaderField: "Accept")

        let token = try await tokenManager.getToken()
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")

        return try await execute(request)
    }

    private func delete(path: String) async throws -> Data {
        let url = baseURL.appendingPathComponent(path)
        var request = URLRequest(url: url)
        request.httpMethod = "DELETE"
        request.setValue("application/json", forHTTPHeaderField: "Accept")

        let token = try await tokenManager.getToken()
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")

        return try await execute(request)
    }

    private func post(path: String, body: [String: Any], authenticated: Bool = true) async throws -> Data {
        let url = baseURL.appendingPathComponent(path)
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)

        if authenticated {
            let token = try await tokenManager.getToken()
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }

        return try await execute(request)
    }

    private func execute(_ request: URLRequest) async throws -> Data {
        let data: Data
        let response: URLResponse

        do {
            (data, response) = try await session.data(for: request)
        } catch let urlError as URLError {
            throw MMError.networkError(underlying: urlError)
        } catch {
            throw MMError.networkError(underlying: error)
        }

        guard let httpResponse = response as? HTTPURLResponse else {
            throw MMError.invalidResponse
        }

        // Success range
        if (200..<300).contains(httpResponse.statusCode) {
            return data
        }

        // Try to decode server error envelope
        if let errorResponse = try? decoder.decode(MMErrorResponse.self, from: data) {
            throw MMError.from(serverResponse: errorResponse)
        }

        // Fallback for non-JSON error responses
        let bodyText = String(data: data, encoding: .utf8) ?? "Unknown error"
        throw MMError.serverError(
            code: "HTTP_\(httpResponse.statusCode)",
            message: bodyText
        )
    }
}
