import 'dart:async';
import 'dart:convert';
import 'dart:html' as html;

import 'package:flutter/foundation.dart';
import 'package:http/http.dart' as http;

/// Web implementation of platform-specific WebRTC operations.
///
/// Uses `dart:html` RtcPeerConnection and MediaStream for direct WebRTC
/// connections to mm-switch. Only compiled on web targets.
class PlatformWebRTC {
  html.RtcPeerConnection? _switchPc;
  html.MediaStream? _switchMediaStream;
  html.RtcPeerConnection? _publisherPc;
  html.MediaStream? _publisherMediaStream;

  Timer? _viewerCountTimer;
  bool _disposed = false;

  /// Callback invoked when connection or track state changes.
  void Function()? onStateChanged;
  bool connected = false;
  bool cameraEnabled = false;
  bool micEnabled = false;

  bool get usesSwitch => _switchPc != null;
  html.MediaStream? get switchMediaStream => _switchMediaStream;
  html.MediaStream? get publisherStream => _publisherMediaStream;

  static const _iceServers = [
    {'urls': 'stun:stun.l.google.com:19302'},
    {
      'urls': 'turn:steegler.com:3478',
      'username': 'mm',
      'credential': 'mm_turn_secret_prod',
    },
  ];

  /// Connect as a viewer via mm-switch (plain WebRTC).
  Future<void> connectViaSwitch(
    String switchUrl,
    String sourceId, {
    String? viewerId,
    String? authToken,
  }) async {
    final id = (viewerId != null && viewerId.isNotEmpty)
        ? viewerId
        : 'v-${DateTime.now().millisecondsSinceEpoch}';
    debugPrint(
        '[PlatformWebRTC] connecting to mm-switch: $switchUrl source=$sourceId viewer=$id');

    final pc = html.RtcPeerConnection({'iceServers': _iceServers});
    _switchPc = pc;

    // Single MediaStream accumulates all incoming tracks.
    final ms = html.MediaStream();
    _switchMediaStream = ms;

    pc.onTrack.listen((e) {
      final track = e.track;
      if (track == null) return;
      debugPrint(
          '[PlatformWebRTC] onTrack: kind=${track.kind} (accumulating into shared stream)');
      ms.addTrack(track);
      onStateChanged?.call();
    });
    pc.onIceConnectionStateChange.listen((_) {
      debugPrint('[PlatformWebRTC] ICE: ${pc.iceConnectionState}');
      connected = pc.iceConnectionState == 'connected' ||
          pc.iceConnectionState == 'completed';
      onStateChanged?.call();
    });

    final offer = await pc.createOffer({
      'offerToReceiveAudio': true,
      'offerToReceiveVideo': true,
    });
    await pc.setLocalDescription({'type': offer.type, 'sdp': offer.sdp});
    await Future.delayed(const Duration(milliseconds: 500)); // ICE gathering

    final localDesc = pc.localDescription;
    final viewerHeaders = <String, String>{'Content-Type': 'application/json'};
    if (authToken != null && authToken.isNotEmpty) {
      viewerHeaders['Authorization'] = 'Bearer $authToken';
    }
    final resp = await http.post(
      Uri.parse('$switchUrl/api/viewers/offer'),
      headers: viewerHeaders,
      body: jsonEncode({
        'id': id,
        'source_id': sourceId,
        'offer': {'type': localDesc?.type, 'sdp': localDesc?.sdp},
      }),
    );
    if (resp.statusCode != 200) {
      throw Exception('mm-switch: ${resp.statusCode}');
    }

    final result = jsonDecode(resp.body) as Map<String, dynamic>;
    final answer = result['answer'] as Map<String, dynamic>;
    await pc.setRemoteDescription(
        {'type': answer['type'], 'sdp': answer['sdp']});
    connected = true;
    debugPrint('[PlatformWebRTC] connected to mm-switch');
    onStateChanged?.call();
  }

  /// Host-side: capture camera+mic and publish directly to mm-switch.
  Future<void> publishToSwitch(String switchUrl, String sourceId, {String? authToken}) async {
    debugPrint(
        '[PlatformWebRTC] publishing to mm-switch: $switchUrl as $sourceId');
    final stream = await html.window.navigator.mediaDevices!.getUserMedia({
      'video': true,
      'audio': true,
    });
    _publisherMediaStream = stream;

    final pc = html.RtcPeerConnection({'iceServers': _iceServers});
    _publisherPc = pc;

    for (final t in stream.getTracks()) {
      pc.addTrack(t, stream);
    }
    pc.onIceConnectionStateChange.listen((_) {
      debugPrint('[PlatformWebRTC] publisher ICE: ${pc.iceConnectionState}');
    });

    final offer = await pc.createOffer();
    await pc.setLocalDescription({'type': offer.type, 'sdp': offer.sdp});
    await Future.delayed(const Duration(milliseconds: 500)); // ICE gathering

    final localDesc = pc.localDescription;
    final pubHeaders = <String, String>{'Content-Type': 'application/json'};
    if (authToken != null && authToken.isNotEmpty) {
      pubHeaders['Authorization'] = 'Bearer $authToken';
    }
    final resp = await http.post(
      Uri.parse('$switchUrl/api/publish/offer'),
      headers: pubHeaders,
      body: jsonEncode({
        'id': sourceId,
        'offer': {'type': localDesc?.type, 'sdp': localDesc?.sdp},
      }),
    );
    if (resp.statusCode != 200) {
      throw Exception(
          'mm-switch publish: ${resp.statusCode} ${resp.body}');
    }
    final result = jsonDecode(resp.body) as Map<String, dynamic>;
    final answer = result['answer'] as Map<String, dynamic>;
    await pc.setRemoteDescription(
        {'type': answer['type'], 'sdp': answer['sdp']});

    cameraEnabled = stream.getVideoTracks().isNotEmpty;
    micEnabled = stream.getAudioTracks().isNotEmpty;
    connected = true;
    debugPrint('[PlatformWebRTC] published to mm-switch as $sourceId');
    onStateChanged?.call();
  }

  /// Host-side: stop the mm-switch publisher.
  void stopPublishing() {
    _publisherMediaStream?.getTracks().forEach((t) => t.stop());
    _publisherPc?.close();
    _publisherPc = null;
    _publisherMediaStream = null;
    cameraEnabled = false;
    micEnabled = false;
    onStateChanged?.call();
  }

  /// Enable/disable microphone on the publisher MediaStream.
  void setMicEnabled(bool enabled) {
    if (_publisherMediaStream != null) {
      for (final t in _publisherMediaStream!.getAudioTracks()) {
        t.enabled = enabled;
      }
    }
  }

  /// Enable/disable camera on the publisher MediaStream.
  void setCameraEnabled(bool enabled) {
    if (_publisherMediaStream != null) {
      for (final t in _publisherMediaStream!.getVideoTracks()) {
        t.enabled = enabled;
      }
    }
  }

  /// Start polling mm-switch for viewer counts.
  void startViewerCountPolling(
    String baseUrl,
    String sourceId,
    void Function(int) onCount,
  ) {
    cancelViewerCountPolling();
    Future<void> tick() async {
      if (_disposed) return;
      try {
        final r = await http.get(Uri.parse('$baseUrl/api/viewers'));
        if (r.statusCode != 200) return;
        final body = jsonDecode(r.body) as Map<String, dynamic>;
        final list = (body['viewers'] as List?) ?? const [];
        int count = 0;
        for (final v in list) {
          if (v is Map) {
            final src = v['current_source'] as String? ?? '';
            if (src == sourceId || src.startsWith('ad-')) count++;
          }
        }
        onCount(count);
      } catch (_) {
        /* ignore transient errors */
      }
    }

    tick();
    _viewerCountTimer =
        Timer.periodic(const Duration(seconds: 3), (_) => tick());
  }

  /// Cancel viewer count polling.
  void cancelViewerCountPolling() {
    _viewerCountTimer?.cancel();
    _viewerCountTimer = null;
  }

  /// Disconnect and clean up all WebRTC resources.
  Future<void> disconnect({bool isHost = false}) async {
    // Publisher (host camera+mic)
    _publisherMediaStream?.getTracks().forEach((t) => t.stop());
    _publisherPc?.close();
    _publisherPc = null;
    _publisherMediaStream = null;
    // Viewer
    _switchPc?.close();
    _switchPc = null;
    _switchMediaStream = null;
    // Polling
    cancelViewerCountPolling();
    // State
    connected = false;
    cameraEnabled = false;
    micEnabled = false;
  }

  void dispose() {
    _disposed = true;
    cancelViewerCountPolling();
    _switchPc?.close();
    _switchPc = null;
    _publisherMediaStream?.getTracks().forEach((t) => t.stop());
    _publisherPc?.close();
    _publisherPc = null;
    _publisherMediaStream = null;
  }
}
