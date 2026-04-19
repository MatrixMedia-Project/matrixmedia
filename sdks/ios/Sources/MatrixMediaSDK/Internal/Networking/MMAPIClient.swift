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
        static func streamRotateKey(id: String) -> String { "\(base)/streams/\(id)/rotate-key" }
        static func roomStreams(roomID: String) -> String { "\(base)/rooms/\(roomID)/streams" }
        static let createStream = "\(base)/streams"
        static func roomRecordings(roomID: String) -> String { "\(base)/rooms/\(roomID)/recordings" }
        static func recording(id: String) -> String { "\(base)/recordings/\(id)" }
        // Donations
        static let donations = "\(base)/donations"
        static func streamDonations(id: String) -> String { "\(base)/streams/\(id)/donations" }
        // Subscriptions & Tiers
        static let creatorOnboard = "\(base)/creator/onboard"
        static let creatorProfile = "\(base)/creator/profile"
        static let creatorTiers = "\(base)/creator/tiers"
        static func creatorTiersList(userId: String) -> String { "\(base)/creators/\(userId)/tiers" }
        static let subscriptions = "\(base)/subscriptions"
        static let subscriptionCheck = "\(base)/subscriptions/check"
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
        if let minTier = config.minTier {
            body["min_tier"] = minTier
        }

        let data = try await post(path: APIPath.createStream, body: body)
        return try decoder.decode(MMCreateStreamResponse.self, from: data)
    }

    /// Fetch the current creator defaults (tier gates + ads_enabled).
    func getCreatorDefaults() async throws -> [String: Any] {
        let data = try await get(path: "\(APIPath.base)/creator/me/defaults")
        return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
    }

    /// Update creator defaults. Server expects the full object — pass any
    /// existing values you don't want to change.
    func updateCreatorDefaults(
        defaultStreamMinTier: Int,
        defaultRecordingMinTier: Int,
        adsEnabled: Bool
    ) async throws -> [String: Any] {
        let body: [String: Any] = [
            "default_stream_min_tier": defaultStreamMinTier,
            "default_recording_min_tier": defaultRecordingMinTier,
            "ads_enabled": adsEnabled,
        ]
        let data = try await put(path: "\(APIPath.base)/creator/me/defaults", body: body)
        return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
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

    private func put(path: String, body: [String: Any], authenticated: Bool = true) async throws -> Data {
        let url = baseURL.appendingPathComponent(path)
        var request = URLRequest(url: url)
        request.httpMethod = "PUT"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)

        if authenticated {
            let token = try await tokenManager.getToken()
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }

        return try await execute(request)
    }

    // MARK: - Participants & Key Rotation

    /// List participants in a stream.
    func listParticipants(streamID: String) async throws -> [[String: Any]] {
        let data = try await get(path: APIPath.streamParticipants(id: streamID))
        let json = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        return json?["participants"] as? [[String: Any]] ?? []
    }

    /// Rotate E2EE key for a stream (host only).
    func rotateKey(streamID: String) async throws {
        _ = try await post(path: APIPath.streamRotateKey(id: streamID), body: [:])
    }

    // MARK: - Donations

    /// Send a donation to a stream.
    func donate(streamID: String, amountCents: Int, message: String? = nil) async throws -> [String: Any] {
        var body: [String: Any] = ["stream_id": streamID, "amount_cents": amountCents]
        if let msg = message, !msg.isEmpty { body["message"] = msg }
        let data = try await post(path: APIPath.donations, body: body)
        return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
    }

    /// Get donation feed for a stream.
    func getDonationFeed(streamID: String) async throws -> [[String: Any]] {
        let data = try await get(path: APIPath.streamDonations(id: streamID))
        let json = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        return json?["donations"] as? [[String: Any]] ?? []
    }

    // MARK: - Creator & Tiers

    /// Onboard as a creator.
    func onboardCreator(displayName: String) async throws -> [String: Any] {
        let data = try await post(path: APIPath.creatorOnboard, body: ["display_name": displayName])
        return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
    }

    /// Get creator profile for the authenticated user.
    func getCreatorProfile() async throws -> [String: Any]? {
        do {
            let data = try await get(path: APIPath.creatorProfile)
            return try JSONSerialization.jsonObject(with: data) as? [String: Any]
        } catch MMError.serverError(let code, _) where code == "MM_NOT_FOUND" || code == "HTTP_404" || code == "HTTP_412" {
            return nil
        }
    }

    /// Create a subscription tier.
    func createTier(name: String, tierLevel: Int, priceCents: Int, perks: [String] = []) async throws -> [String: Any] {
        let body: [String: Any] = ["name": name, "tier_level": tierLevel, "price_cents": priceCents, "perks": perks]
        let data = try await post(path: APIPath.creatorTiers, body: body)
        return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
    }

    /// List subscription tiers for a creator.
    func listCreatorTiers(creatorUserID: String) async throws -> [[String: Any]] {
        let encoded = creatorUserID.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? creatorUserID
        let data = try await get(path: APIPath.creatorTiersList(userId: encoded))
        let json = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        return json?["tiers"] as? [[String: Any]] ?? []
    }

    // MARK: - Subscriptions

    /// Subscribe to a tier.
    func subscribe(tierID: String) async throws -> [String: Any] {
        let data = try await post(path: APIPath.subscriptions, body: ["tier_id": tierID])
        return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
    }

    /// Check entitlement for a creator.
    func checkEntitlement(creatorUserID: String) async throws -> [String: Any] {
        let encoded = creatorUserID.addingPercentEncoding(withAllowedCharacters: .urlQueryAllowed) ?? creatorUserID
        let data = try await get(path: "\(APIPath.subscriptionCheck)?creator_user_id=\(encoded)")
        return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
    }

    // MARK: - Advertising

    /// Get an ad decision for a stream (pre-roll, mid-roll, etc.).
    func getAdDecision(streamID: String, slot: String = "pre_roll") async throws -> [String: Any] {
        let data = try await get(path: "\(APIPath.base)/streams/\(streamID)/ad-decision?slot=\(slot)")
        return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
    }

    /// Submit ad completion proof (HMAC challenge-response).
    func submitAdComplete(streamID: String, impressionToken: String, challengeResponse: String, timestamp: Int) async throws {
        let body: [String: Any] = [
            "impression_token": impressionToken,
            "challenge_response": challengeResponse,
            "timestamp": timestamp,
        ]
        _ = try await post(path: "\(APIPath.base)/streams/\(streamID)/ad-complete", body: body)
    }

    /// Report an ad event (quartile progress, click, skip, error).
    func reportAdEvent(impressionToken: String, event: String, positionSecs: Int? = nil) async throws {
        var body: [String: Any] = [
            "impression_token": impressionToken,
            "event": event,
        ]
        if let pos = positionSecs {
            body["position_secs"] = pos
        }
        _ = try await post(path: "\(APIPath.base)/ads/events", body: body)
    }

    /// Check if the viewer is currently in an ad break.
    func getAdStatus(streamID: String) async throws -> Bool {
        let data = try await get(path: "\(APIPath.base)/streams/\(streamID)/ad-status")
        let json = (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
        return json["in_ad_break"] as? Bool ?? false
    }

    /// Upload an ad creative.
    func uploadAd(title: String, placement: String, durationSecs: Int = 15) async throws -> [String: Any] {
        let body: [String: Any] = [
            "title": title,
            "placement": placement,
            "duration_secs": durationSecs,
        ]
        let data = try await post(path: "\(APIPath.base)/ads", body: body)
        return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
    }

    /// List my ads.
    func listMyAds() async throws -> [[String: Any]] {
        let data = try await get(path: "\(APIPath.base)/ads")
        let json = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        return json?["ads"] as? [[String: Any]] ?? []
    }

    /// Delete an ad.
    func deleteAd(adID: String) async throws {
        _ = try await delete(path: "\(APIPath.base)/ads/\(adID)")
    }

    /// Get ad statistics.
    func getAdStats(adID: String) async throws -> [String: Any] {
        let data = try await get(path: "\(APIPath.base)/ads/\(adID)/stats")
        return (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
    }

    /// Trigger mid-roll ad break (host only).
    func triggerAdBreak(streamID: String) async throws {
        _ = try await post(path: "\(APIPath.base)/streams/\(streamID)/ad-break", body: [:])
    }

    // MARK: - Private helpers

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
