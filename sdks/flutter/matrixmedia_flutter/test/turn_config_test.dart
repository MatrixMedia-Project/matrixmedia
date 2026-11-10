import 'package:flutter_test/flutter_test.dart';

import 'package:matrixmedia_flutter/src/turn_config.dart';

void main() {
  test(
    'ICE servers keep UDP first with a TCP fallback on the same TURN host',
    () {
      expect(turnIceServers.length, 3);
      expect(turnIceServers[0]['urls'], 'stun:stun.l.google.com:19302');

      final udp = turnIceServers[1];
      final tcp = turnIceServers[2];
      expect(udp['urls'], 'turn:steegler.com:3478');
      expect(tcp['urls'], 'turn:steegler.com:3478?transport=tcp');
      // Same host and credentials — the TCP entry is a fallback, not a
      // different server.
      expect(tcp['username'], udp['username']);
      expect(tcp['credential'], udp['credential']);
    },
  );
}
