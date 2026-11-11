import 'package:flutter_test/flutter_test.dart';

import 'package:matrixmedia_flutter/src/publish_watchdog.dart';

void main() {
  group('sumOutboundBytesSent', () {
    test('sums audio+video outbound-rtp bytesSent', () {
      const stats = [
        {'type': 'outbound-rtp', 'kind': 'audio', 'bytesSent': 1200},
        {'type': 'outbound-rtp', 'kind': 'video', 'bytesSent': 34500},
        {'type': 'inbound-rtp', 'kind': 'video', 'bytesSent': 999},
        {'type': 'candidate-pair', 'bytesSent': 50},
      ];
      expect(sumOutboundBytesSent(stats), 35700);
    });

    test('ignores missing or non-numeric bytesSent', () {
      const stats = [
        {'type': 'outbound-rtp', 'kind': 'video'},
        {'type': 'outbound-rtp', 'kind': 'audio', 'bytesSent': 'x'},
        {'type': 'outbound-rtp', 'kind': 'video', 'bytesSent': 100},
      ];
      expect(sumOutboundBytesSent(stats), 100);
    });

    test('empty report sums to zero', () {
      expect(sumOutboundBytesSent(const []), 0);
    });
  });

  group('PublishWatchdog', () {
    test('steady growth never requests a restart', () {
      final w = PublishWatchdog(stallTicks: 4, maxRestarts: 3);
      var bytes = 0;
      for (var i = 0; i < 20; i++) {
        bytes += 1000;
        expect(w.sample(bytes), WatchdogAction.ok);
      }
      expect(w.restarts, 0);
    });

    test('flat bytes for stallTicks requests one restart', () {
      final w = PublishWatchdog(stallTicks: 4, maxRestarts: 3);
      expect(w.sample(5000), WatchdogAction.ok);
      expect(w.sample(5000), WatchdogAction.ok);
      expect(w.sample(5000), WatchdogAction.ok);
      expect(w.sample(5000), WatchdogAction.restart);
      expect(w.restarts, 1);
    });

    test('growth after restart resets the stall counter', () {
      final w = PublishWatchdog(stallTicks: 2, maxRestarts: 3);
      expect(w.sample(100), WatchdogAction.ok);
      expect(w.sample(100), WatchdogAction.restart);
      expect(w.sample(200), WatchdogAction.ok);
      expect(w.sample(200), WatchdogAction.ok);
      expect(w.sample(200), WatchdogAction.restart);
      expect(w.restarts, 2);
    });

    test('reports failed after maxRestarts with no recovery', () {
      final w = PublishWatchdog(stallTicks: 1, maxRestarts: 2);
      expect(w.sample(0), WatchdogAction.restart);
      expect(w.sample(0), WatchdogAction.restart);
      expect(w.sample(0), WatchdogAction.failed);
      expect(w.exhausted, isTrue);
      // Stays failed until bytes grow again.
      expect(w.sample(0), WatchdogAction.failed);
      expect(w.sample(1), WatchdogAction.ok);
      expect(w.exhausted, isTrue);
    });

    test('reset clears counters', () {
      final w = PublishWatchdog(stallTicks: 1, maxRestarts: 1);
      expect(w.sample(0), WatchdogAction.restart);
      expect(w.sample(0), WatchdogAction.failed);
      w.reset();
      expect(w.exhausted, isFalse);
      expect(w.restarts, 0);
      expect(w.sample(0), WatchdogAction.restart);
    });
  });
}
