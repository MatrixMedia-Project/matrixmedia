import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:matrixmedia_flutter/src/live_captions.dart';

String _batch(List<Map<String, Object?>> segments) =>
    jsonEncode({'type': 'caption', 'segments': segments});

void main() {
  group('parseCaptionBatch', () {
    test('parses interim and final segments with language', () {
      final batch = parseCaptionBatch(
        _batch([
          {'id': 's1', 'text': 'hello', 'final': false, 'language': 'en'},
          {'id': 's0', 'text': 'intro', 'final': true},
        ]),
      );
      expect(batch, hasLength(2));
      expect(
        batch!.first,
        const LiveCaptionSegment(
          id: 's1',
          text: 'hello',
          language: 'en',
          isFinal: false,
        ),
      );
      expect(batch.last.isFinal, isTrue);
      expect(batch.last.language, isEmpty);
    });

    test('ended control signals and other types are ignored', () {
      expect(parseCaptionBatch('{"type":"ended"}'), isNull);
      expect(parseCaptionBatch('{"type":"captioned","segments":[]}'), isNull);
    });

    test('garbage and non-map payloads are ignored', () {
      expect(parseCaptionBatch('not json'), isNull);
      expect(parseCaptionBatch('[1,2]'), isNull);
      expect(parseCaptionBatch('{"type":"caption"}'), isNull);
    });

    test('blank lines are dropped, bad entries skipped', () {
      final batch = parseCaptionBatch(
        _batch([
          {'id': 's1', 'text': '   ', 'final': true},
          {'id': 's2', 'final': true},
          {'text': 'no id', 'final': true},
          {'id': 's3', 'text': 'kept', 'final': true},
        ]),
      );
      expect(batch, hasLength(1));
      expect(batch!.single.id, 's3');
    });
  });
}
