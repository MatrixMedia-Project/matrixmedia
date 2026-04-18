import 'dart:html' as html;
import 'dart:ui_web' as ui_web;

import 'package:flutter/material.dart';

/// Builds an mm-switch video widget using an HTML <video> element.
/// Web-only; the stub version returns SizedBox.shrink() on mobile.
Widget buildSwitchVideo(dynamic mediaStream, {bool alwaysMuted = false}) {
  return _SwitchVideo(
    mediaStream: mediaStream as html.MediaStream,
    alwaysMuted: alwaysMuted,
  );
}

/// Renders a raw html.MediaStream from mm-switch via HTML <video> element.
/// Holds a reference to the created VideoElement so we can update srcObject
/// when the upstream MediaStream changes (e.g., source switch).
class _SwitchVideo extends StatefulWidget {
  final html.MediaStream mediaStream;

  /// When true, the <video> stays permanently muted (host self-preview).
  final bool alwaysMuted;
  const _SwitchVideo({required this.mediaStream, this.alwaysMuted = false});

  @override
  State<_SwitchVideo> createState() => _SwitchVideoState();
}

class _SwitchVideoState extends State<_SwitchVideo> {
  late final String _viewType;
  html.VideoElement? _videoEl;

  @override
  void initState() {
    super.initState();
    _viewType = 'mm-sw-${DateTime.now().millisecondsSinceEpoch}';
    ui_web.platformViewRegistry.registerViewFactory(_viewType, (int viewId) {
      final video = html.VideoElement()
        ..autoplay = true
        ..muted = true // muted autoplay is always allowed
        ..setAttribute('playsinline', 'true')
        ..style.width = '100%'
        ..style.height = '100%'
        ..style.objectFit = 'cover'
        ..style.backgroundColor = 'black'
        ..srcObject = widget.mediaStream;

      _videoEl = video;
      debugPrint(
          '[_SwitchVideo] <video> created, tracks=${widget.mediaStream.getVideoTracks().length}');

      video.play().catchError((e) {
        debugPrint('[_SwitchVideo] play() failed: $e');
      });
      // Unmute after 500ms for viewers so they hear audio.
      // Host self-preview stays permanently muted to prevent echo.
      if (!widget.alwaysMuted) {
        Future.delayed(const Duration(milliseconds: 500), () {
          video.muted = false;
          video.play().catchError((_) {});
        });
      }
      return video;
    });
  }

  @override
  void didUpdateWidget(_SwitchVideo oldWidget) {
    super.didUpdateWidget(oldWidget);
    // The upstream stream object can change (e.g., source switch creates a
    // new MediaStream). If it does, point the video element at the new one.
    if (_videoEl != null && oldWidget.mediaStream != widget.mediaStream) {
      _videoEl!.srcObject = widget.mediaStream;
      _videoEl!.play().catchError((_) {});
      debugPrint(
          '[_SwitchVideo] srcObject updated, tracks=${widget.mediaStream.getVideoTracks().length}');
    }
  }

  @override
  Widget build(BuildContext context) {
    return HtmlElementView(viewType: _viewType);
  }
}
