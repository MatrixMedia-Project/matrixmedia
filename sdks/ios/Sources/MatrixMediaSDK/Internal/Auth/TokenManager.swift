import Foundation

/// Manages the MM session JWT lifecycle: authentication, caching, and auto-refresh.
///
/// ## Design (PTT Knowledge Base lesson #9)
///
/// The TokenManager holds two types of tokens:
/// 1. **Matrix OpenID token** -- obtained via the `tokenProvider` closure. NEVER cached;
///    the closure is called fresh each time authentication is needed.
/// 2. **MM session JWT** -- obtained by exchanging the OpenID token with mm-core.
///    Cached internally with auto-refresh at 80% of TTL.
///
/// The `getToken()` method is the single entry point for all authenticated API calls.
/// It returns a valid session JWT, refreshing transparently if needed.
final class TokenManager: @unchecked Sendable {

    // MARK: - Constants

    /// Refresh the token when 80% of its TTL has elapsed.
    /// For a 15-minute JWT, refresh happens at 12 minutes.
    private static let refreshThreshold: Double = 0.80

    // MARK: - Properties

    private let serverURL: URL
    private let tokenProvider: @Sendable () async throws -> MMOpenIDToken
    private let lock = NSLock()

    /// Current session state, protected by `lock`.
    private var currentToken: MMAuthToken?
    private var userId: String?
    private var displayName: String?

    /// Prevents concurrent refresh attempts.
    private var refreshTask: Task<MMAuthToken, Error>?

    // MARK: - Lazy API Client

    /// Lazy reference to avoid circular dependency (MMAPIClient holds TokenManager).
    /// Set by MMAPIClient after construction.
    private var _apiClient: MMAPIClient?

    var apiClient: MMAPIClient {
        get {
            guard let client = _apiClient else {
                fatalError("TokenManager.apiClient not set. This is a programming error.")
            }
            return client
        }
        set { _apiClient = newValue }
    }

    // MARK: - Init

    init(serverURL: URL, tokenProvider: @escaping @Sendable () async throws -> MMOpenIDToken) {
        self.serverURL = serverURL
        self.tokenProvider = tokenProvider
    }

    // MARK: - Public API

    /// Authenticate with mm-core by obtaining a fresh OpenID token and exchanging it.
    ///
    /// - Returns: The authenticated ``MMUser``.
    func authenticate() async throws -> MMUser {
        // 1. Get a fresh OpenID token (never cached)
        let openIDToken: MMOpenIDToken
        do {
            openIDToken = try await tokenProvider()
        } catch {
            throw MMError.notAuthenticated
        }

        // 2. Exchange for MM session JWT
        let authToken = try await apiClient.exchangeToken(openIDToken: openIDToken)

        // 3. Store session
        lock.lock()
        currentToken = authToken
        userId = authToken.userId
        lock.unlock()

        return MMUser(
            id: authToken.userId,
            displayName: nil,
            avatarURL: nil,
            homeserver: openIDToken.matrixServerName
        )
    }

    /// Get a valid session JWT for API requests.
    ///
    /// If the current token is expired or about to expire, transparently refreshes it.
    /// If refresh fails, re-authenticates from scratch via the token provider.
    ///
    /// - Returns: A valid MM session JWT string.
    func getToken() async throws -> String {
        lock.lock()
        guard let token = currentToken else {
            lock.unlock()
            throw MMError.notAuthenticated
        }

        // Check if token needs refresh (80% of TTL elapsed)
        let now = Date()
        let ttl = token.expiresAt.timeIntervalSince(now)
        let totalTTL = token.expiresAt.timeIntervalSince(
            token.expiresAt.addingTimeInterval(-900) // Assume 15-min TTL for threshold
        )
        let threshold = totalTTL * Self.refreshThreshold

        if ttl > (totalTTL - threshold) && ttl > 0 {
            // Token is still valid and not near expiry
            let jwt = token.sessionToken
            lock.unlock()
            return jwt
        }

        // Token needs refresh
        if let existing = refreshTask {
            lock.unlock()
            let refreshed = try await existing.value
            return refreshed.sessionToken
        }

        let refreshTokenValue = token.refreshToken
        let task = Task<MMAuthToken, Error> {
            defer {
                self.lock.lock()
                self.refreshTask = nil
                self.lock.unlock()
            }

            do {
                // Try refresh first
                let newToken = try await self.apiClient.refreshToken(refreshToken: refreshTokenValue)
                let updatedToken = MMAuthToken(
                    sessionToken: newToken.sessionToken,
                    refreshToken: newToken.refreshToken,
                    expiresAt: newToken.expiresAt,
                    userId: self.userId ?? ""
                )
                self.lock.lock()
                self.currentToken = updatedToken
                self.lock.unlock()
                return updatedToken
            } catch {
                // Refresh failed -- try full re-authentication
                do {
                    let openIDToken = try await self.tokenProvider()
                    let authToken = try await self.apiClient.exchangeToken(openIDToken: openIDToken)
                    self.lock.lock()
                    self.currentToken = authToken
                    self.userId = authToken.userId
                    self.lock.unlock()
                    return authToken
                } catch {
                    self.lock.lock()
                    self.currentToken = nil
                    self.lock.unlock()
                    throw MMError.tokenRefreshFailed(underlying: error)
                }
            }
        }

        refreshTask = task
        lock.unlock()
        let result = try await task.value
        return result.sessionToken
    }

    /// Clear all stored session state.
    func clearSession() {
        lock.lock()
        currentToken = nil
        userId = nil
        displayName = nil
        refreshTask?.cancel()
        refreshTask = nil
        lock.unlock()
    }

    /// Whether the manager currently holds a session token (may be expired).
    var hasSession: Bool {
        lock.lock()
        defer { lock.unlock() }
        return currentToken != nil
    }
}
