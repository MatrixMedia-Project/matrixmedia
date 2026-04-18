import 'package:flutter/material.dart';
import 'package:flutter_webrtc/flutter_webrtc.dart';

/// Mobile: render an mm-switch MediaStream using flutter_webrtc's RTCVideoView.
Widget buildSwitchVideo(dynamic mediaStream, {bool alwaysMuted = false}) {
  if (mediaStream == null || mediaStream is! MediaStream) {
    return const SizedBox.shrink();
  }
  return _SwitchVideoMobile(
    mediaStream: mediaStream,
    alwaysMuted: alwaysMuted,
  );
}

class _SwitchVideoMobile extends StatefulWidget {
  final MediaStream mediaStream;
  final bool alwaysMuted;

  const _SwitchVideoMobile({
    required this.mediaStream,
    this.alwaysMuted = false,
  });

  @override
  State<_SwitchVideoMobile> createState() => _SwitchVideoMobileState();
}

class _SwitchVideoMobileState extends State<_SwitchVideoMobile> {
  final RTCVideoRenderer _renderer = RTCVideoRenderer();
  bool _initialized = false;

  @override
  void initState() {
    super.initState();
    _init();
  }

  Future<void> _init() async {
    await _renderer.initialize();
    _renderer.srcObject = widget.mediaStream;
    if (widget.alwaysMuted) {
      // Mute audio tracks so the host doesn't hear their own mic.
      for (final t in widget.mediaStream.getAudioTracks()) {
        t.enabled = false;
      }
    }
    if (mounted) setState(() => _initialized = true);
  }

  @override
  void didUpdateWidget(covariant _SwitchVideoMobile old) {
    super.didUpdateWidget(old);
    if (old.mediaStream != widget.mediaStream) {
      _renderer.srcObject = widget.mediaStream;
    }
  }

  @override
  void dispose() {
    _renderer.srcObject = null;
    _renderer.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    if (!_initialized) {
      return const Center(child: CircularProgressIndicator());
    }
    return RTCVideoView(
      _renderer,
      objectFit: RTCVideoViewObjectFit.RTCVideoViewObjectFitContain,
      mirror: false,
    );
  }
}
