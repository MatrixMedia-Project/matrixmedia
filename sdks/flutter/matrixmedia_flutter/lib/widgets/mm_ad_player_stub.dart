import 'package:flutter/material.dart';

/// Platform ad video helper for mobile (stub).
/// On mobile, no HTML video element is available.
class PlatformAdVideo {
  /// Always null on mobile -- no video element.
  dynamic get videoElement => null;

  /// No-op on mobile.
  void pause() {}
}

/// Create and register a platform view for ad playback.
/// On mobile, returns a PlatformAdVideo with no backing element.
PlatformAdVideo createAdVideoElement({
  required String mediaUrl,
  required String viewType,
  required VoidCallback onEnded,
  required VoidCallback onError,
}) {
  return PlatformAdVideo();
}

/// Open a click-through URL. No-op on mobile.
void openClickThrough(String url) {}
