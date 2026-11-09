import 'package:flutter_test/flutter_test.dart';
import 'package:matrixmedia_flutter/src/mm_types.dart';

/// Real recording-API wire shape: `caption_urls` lists one servable WebVTT
/// URL per ready language, plus the source-language `summary` convenience.
Map<String, dynamic> _recordingJson({
  Object? captionUrls = const {
    'en': 'https://cdn.example/r1.en.vtt',
    'de': 'https://cdn.example/r1.de.vtt',
  },
  Object? summary = 'A show about testing.',
}) => {
  'id': 'rec1',
  'stream_id': 's1',
  'host_user_id': '@a:x.y',
  'media_type': 'video',
  'title': 'Show',
  'status': 'ready',
  'duration_ms': 30000,
  'size_bytes': 1024,
  'playback_url': 'https://cdn.example/r1.mp4',
  'mxc_url': null,
  'created_at': '2026-11-09T00:00:00Z',
  'caption_urls': captionUrls,
  'summary': summary,
};

void main() {
  group('MMRecording caption fields', () {
    test('caption_urls and summary parse when present', () {
      final recording = MMRecording.fromJson(_recordingJson());
      expect(recording.captionUrls, {
        'en': 'https://cdn.example/r1.en.vtt',
        'de': 'https://cdn.example/r1.de.vtt',
      });
      expect(recording.summary, 'A show about testing.');
    });

    test('ready English track is listed and selectable', () {
      final recording = MMRecording.fromJson(_recordingJson());
      expect(recording.captionUrls, contains('en'));
      expect(recording.captionUrls['en'], endsWith('.en.vtt'));
    });

    test('missing caption fields default to empty and null', () {
      final json = _recordingJson()
        ..remove('caption_urls')
        ..remove('summary');
      final recording = MMRecording.fromJson(json);
      expect(recording.captionUrls, isEmpty);
      expect(recording.summary, isNull);
    });

    test('null and non-map caption payloads yield an empty map', () {
      expect(
        MMRecording.fromJson(_recordingJson(captionUrls: null)).captionUrls,
        isEmpty,
      );
      expect(
        MMRecording.fromJson(
          _recordingJson(captionUrls: 'https://cdn.example/r1.en.vtt'),
        ).captionUrls,
        isEmpty,
      );
    });

    test('null track URLs are skipped', () {
      final recording = MMRecording.fromJson(
        _recordingJson(
          captionUrls: {'en': 'https://cdn.example/r1.en.vtt', 'fr': null},
        ),
      );
      expect(recording.captionUrls, {'en': 'https://cdn.example/r1.en.vtt'});
    });
  });
}
