/// Viewer live captions arriving on the mm-switch `mm-control` data channel.
///
// Wire format (mirrors iOS `LiveCaptionWireMessage` and
// `services/mm-switch/live_captions.go:captionWireMessage`):
//   `{"type":"caption","segments":[{"id","text","final","language"?}]}`
// Interim segments (`final=false`) render lighter; `final=true` segments
// persist to VOD server-side. Blank lines are noise and dropped at parse.
library;

import 'dart:convert';

/// One live caption line, transport-agnostic.
class LiveCaptionSegment {
  final String id;
  final String text;

  /// BCP-47 tag from the STT worker; empty when the server omits it.
  final String language;
  final bool isFinal;

  const LiveCaptionSegment({
    required this.id,
    required this.text,
    this.language = '',
    required this.isFinal,
  });

  factory LiveCaptionSegment.fromJson(Map<String, dynamic> json) {
    return LiveCaptionSegment(
      id: '${json['id']}',
      text: '${json['text'] ?? ''}',
      language: json['language']?.toString() ?? '',
      isFinal: json['final'] == true,
    );
  }

  @override
  bool operator ==(Object other) =>
      other is LiveCaptionSegment &&
      other.id == id &&
      other.text == text &&
      other.language == language &&
      other.isFinal == isFinal;

  @override
  int get hashCode => Object.hash(id, text, language, isFinal);
}

/// Decodes one mm-switch mm-control message. Returns the caption batch, or
/// `null` for anything that is not caption traffic (`ended` control signals
/// and unrelated messages are ignored here so the listener stays
/// caption-scoped).
List<LiveCaptionSegment>? parseCaptionBatch(String message) {
  final Object? decoded;
  try {
    decoded = jsonDecode(message);
  } on FormatException {
    return null;
  }
  if (decoded is! Map<String, dynamic>) return null;
  if (decoded['type'] != 'caption') return null;
  final segments = decoded['segments'];
  if (segments is! List) return null;
  final batch = <LiveCaptionSegment>[];
  for (final entry in segments) {
    if (entry is! Map<String, dynamic>) continue;
    if (entry['id'] == null || entry['text'] == null) continue;
    final segment = LiveCaptionSegment.fromJson(entry);
    if (segment.text.trim().isEmpty) continue;
    batch.add(segment);
  }
  return batch;
}
