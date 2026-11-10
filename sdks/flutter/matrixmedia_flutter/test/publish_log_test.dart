import 'package:flutter_test/flutter_test.dart';

import 'package:matrixmedia_flutter/src/publish_log.dart';

const _sendonlyOffer = '''v=0
o=- 1 2 IN IP4 0.0.0.0
m=audio 9 UDP/TLS/RTP/SAVPF 109
a=sendonly
a=msid:mm-switch mm-audio
m=video 9 UDP/TLS/RTP/SAVPF 96
a=sendonly
a=msid:mm-switch mm-video-vp8
''';

const _recvonlyOffer = '''v=0
o=- 3 4 IN IP4 0.0.0.0
m=audio 9 UDP/TLS/RTP/SAVPF 109
a=recvonly
m=video 9 UDP/TLS/RTP/SAVPF 96
a=recvonly
''';

void main() {
  group('describePublishCapture', () {
    test('healthy publish shows tracks and senders', () {
      final line = describePublishCapture(
        trackKinds: ['audio', 'video'],
        offerSdp: _sendonlyOffer,
      );
      expect(line, contains('tracks=2'));
      expect(line, contains('m-lines=[audio,video]'));
      expect(line, contains('senders=2'));
    });

    test('empty capture with recvonly offer is unmistakable', () {
      final line = describePublishCapture(
        trackKinds: [],
        offerSdp: _recvonlyOffer,
      );
      expect(line, contains('tracks=0'));
      expect(line, contains('senders=0'));
    });
  });
}
