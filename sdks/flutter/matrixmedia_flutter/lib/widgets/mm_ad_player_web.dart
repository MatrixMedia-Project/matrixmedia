import 'dart:html' as html;
import 'dart:ui_web' as ui_web;

import 'package:flutter/material.dart';

/// Platform ad video helper for web.
/// Wraps an HTML VideoElement created via platformViewRegistry.
class PlatformAdVideo {
  html.VideoElement? _video;

  /// The underlying HTML video element (available after the platform view
  /// factory has been called, i.e., after HtmlElementView builds).
  dynamic get videoElement => _video;

  void pause() {
    _video?.pause();
  }
}

/// Create and register an HTML video element for ad playback (web-only).
/// Returns a [PlatformAdVideo] whose [videoElement] is populated once the
/// platform view factory runs.
PlatformAdVideo createAdVideoElement({
  required String mediaUrl,
  required String viewType,
  required VoidCallback onEnded,
  required VoidCallback onError,
}) {
  final holder = PlatformAdVideo();
  ui_web.platformViewRegistry.registerViewFactory(viewType, (int viewId) {
    final video = html.VideoElement()
      ..src = mediaUrl
      ..autoplay = true
      ..controls = false
      ..style.width = '100%'
      ..style.height = '100%'
      ..style.backgroundColor = 'black'
      ..style.objectFit = 'contain'
      ..setAttribute('playsinline', 'true');

    video.onEnded.listen((_) => onEnded());
    video.onError.listen((_) => onError());

    holder._video = video;
    return video;
  });
  return holder;
}

/// Open a click-through URL in a new tab (web-only).
void openClickThrough(String url) {
  html.window.open(url, '_blank');
}
