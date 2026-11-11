/// Client-side publisher watchdog: detects a dead WebRTC uplink
/// (ICE "connected" but zero RTP flowing to mm-switch) and decides when
/// the publisher should restart with the same stream id.
///
/// Pure Dart so both platform implementations (web `dart:html` and
/// mobile `flutter_webrtc`) share the same policy, and so the policy
/// is unit-testable without a real peer connection.
library;

/// Action the watchdog recommends after feeding one `getStats` sample.
enum WatchdogAction {
  /// Uplink is healthy (or not yet proven stalled).
  ok,

  /// No outbound growth for [PublishWatchdog.stallTicks] samples:
  /// close the publisher peer connection and re-publish the same
  /// stream id (attempt [PublishWatchdog.restarts] of `maxRestarts`).
  restart,

  /// Still stalled after [PublishWatchdog.maxRestarts] restarts:
  /// stop retrying and surface the failure to the UI.
  failed,
}

/// Sums `bytesSent` across `outbound-rtp` stats reports.
///
/// Accepts raw report maps from either platform: web (`dart:html`
/// `RtcStatsReport` values) or mobile (`StatsReport.values`). Entries
/// without a numeric `bytesSent` (inbound, candidate-pair, remote)
/// contribute zero.
int sumOutboundBytesSent(Iterable<Map> reports) {
  var total = 0;
  for (final report in reports) {
    if (report['type'] != 'outbound-rtp') continue;
    final bytes = report['bytesSent'];
    if (bytes is num) total += bytes.toInt();
  }
  return total;
}

/// Restart policy for one publisher session.
///
/// Feed cumulative outbound `bytesSent` via [sample]. Any growth resets
/// the stall counter; flat readings for [stallTicks] in a row request a
/// restart until [maxRestarts] is reached, then report [WatchdogAction.failed].
class PublishWatchdog {
  PublishWatchdog({this.stallTicks = 4, this.maxRestarts = 3});

  /// Consecutive flat samples required before a restart is requested.
  /// Default 4 x 2.5s sampling ~= ~10s of dead uplink.
  final int stallTicks;

  /// Maximum restarts before giving up and reporting failure.
  final int maxRestarts;

  int _flatTicks = 0;
  int _restarts = 0;
  int? _lastBytes;

  /// Restarts requested so far in this session.
  int get restarts => _restarts;

  /// True once [maxRestarts] restarts were used without recovery.
  bool get exhausted => _restarts >= maxRestarts;

  WatchdogAction sample(int bytesSent) {
    if (_lastBytes != null && bytesSent > _lastBytes!) {
      _flatTicks = 0;
      _lastBytes = bytesSent;
      return WatchdogAction.ok;
    }
    _lastBytes = bytesSent;
    _flatTicks++;
    if (_flatTicks < stallTicks) return WatchdogAction.ok;
    if (exhausted) return WatchdogAction.failed;
    _flatTicks = 0;
    _restarts++;
    return WatchdogAction.restart;
  }

  /// Clears all counters (call when a new publisher session starts).
  void reset() {
    _flatTicks = 0;
    _restarts = 0;
    _lastBytes = null;
  }
}
