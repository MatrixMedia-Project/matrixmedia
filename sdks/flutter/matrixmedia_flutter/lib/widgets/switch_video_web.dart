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
        ..muted =
            true // muted autoplay is always allowed
        ..setAttribute('playsinline', 'true')
        ..style.width = '100%'
        ..style.height = '100%'
        ..style.objectFit = 'cover'
        ..style.backgroundColor = 'black'
        ..srcObject = widget.mediaStream;

      _videoEl = video;
      debugPrint(
        '[_SwitchVideo] <video> created, tracks=${widget.mediaStream.getVideoTracks().length}',
      );

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
    // Always re-attach, not only when the stream object identity changes:
    // viewers accumulate remote tracks into ONE shared MediaStream, so a
    // source swap (pre-roll ad -> stream, or a re-watch's new tracks)
    // mutates the same object. An identity check skipped the re-attach
    // and left the <video> bound to the ended/empty track set — black
    // and silent even though RTP kept arriving. Re-assigning srcObject
    // and calling play() re-binds the element to the live track set.
    final video = _videoEl;
    if (video != null) {
      video.srcObject = widget.mediaStream;
      video.play().catchError((_) {});
      debugPrint(
        '[_SwitchVideo] srcObject re-attached, tracks=${widget.mediaStream.getVideoTracks().length}',
      );
    }
  }

  @override
  void dispose() {
    // Detach so a later watch creates an element bound to the new
    // session's stream, not this session's ended tracks.
    _videoEl?.srcObject = null;
    _videoEl = null;
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return HtmlElementView(viewType: _viewType);
  }
}
