import Foundation
#if canImport(Network)
import Network
#endif

/// Exponential backoff reconnection policy with jitter and network change detection.
///
/// Uses `NWPathMonitor` (when available) to detect network transitions and trigger
/// immediate reconnection attempts when connectivity is restored, rather than
/// waiting for the next backoff interval.
///
/// ## Parameters
/// - Base delay: 1 second
/// - Max delay: 30 seconds
/// - Max retries: 10
/// - Jitter: +/- 25% of computed delay
final class ReconnectPolicy: @unchecked Sendable {

    // MARK: - Configuration

    /// Base delay for the first retry.
    let baseDelay: TimeInterval

    /// Maximum delay between retries.
    let maxDelay: TimeInterval

    /// Maximum number of retry attempts before giving up.
    let maxRetries: Int

    // MARK: - State

    private(set) var currentAttempt: Int = 0
    private var isCancelled = false

    #if canImport(Network)
    private var pathMonitor: NWPathMonitor?
    private let monitorQueue = DispatchQueue(label: "com.matrixmedia.sdk.reconnect")
    #endif

    /// Called when the network path changes to "satisfied" (connectivity restored).
    var onNetworkRestored: (() -> Void)?

    // MARK: - Init

    init(
        baseDelay: TimeInterval = 1.0,
        maxDelay: TimeInterval = 30.0,
        maxRetries: Int = 10
    ) {
        self.baseDelay = baseDelay
        self.maxDelay = maxDelay
        self.maxRetries = maxRetries
    }

    deinit {
        stopMonitoring()
    }

    // MARK: - Backoff Calculation

    /// Returns the delay for the current attempt, or `nil` if max retries exceeded.
    func nextDelay() -> TimeInterval? {
        guard currentAttempt < maxRetries else { return nil }
        defer { currentAttempt += 1 }

        // Exponential backoff: base * 2^attempt
        let exponential = baseDelay * pow(2.0, Double(currentAttempt))
        let capped = min(exponential, maxDelay)

        // Add jitter: +/- 25%
        let jitterRange = capped * 0.25
        let jitter = Double.random(in: -jitterRange...jitterRange)

        return max(0, capped + jitter)
    }

    /// Wait for the next backoff interval.
    ///
    /// - Returns: `true` if the wait completed (proceed with retry),
    ///   `false` if cancelled or max retries exceeded.
    func waitForNextRetry() async -> Bool {
        guard let delay = nextDelay() else { return false }

        do {
            try await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
            return !isCancelled
        } catch {
            return false
        }
    }

    /// Reset the retry counter (e.g., after a successful connection).
    func reset() {
        currentAttempt = 0
        isCancelled = false
    }

    /// Cancel any pending retry.
    func cancel() {
        isCancelled = true
        stopMonitoring()
    }

    // MARK: - Network Monitoring

    /// Start monitoring network path changes.
    ///
    /// When connectivity is restored after a loss, `onNetworkRestored` is called
    /// to trigger an immediate reconnection attempt.
    func startMonitoring() {
        #if canImport(Network)
        let monitor = NWPathMonitor()
        pathMonitor = monitor

        var wasUnsatisfied = false

        monitor.pathUpdateHandler = { [weak self] path in
            if path.status == .satisfied && wasUnsatisfied {
                // Network restored -- trigger immediate retry
                self?.currentAttempt = 0
                self?.onNetworkRestored?()
            }
            wasUnsatisfied = path.status != .satisfied
        }

        monitor.start(queue: monitorQueue)
        #endif
    }

    /// Stop network monitoring.
    func stopMonitoring() {
        #if canImport(Network)
        pathMonitor?.cancel()
        pathMonitor = nil
        #endif
    }
}
