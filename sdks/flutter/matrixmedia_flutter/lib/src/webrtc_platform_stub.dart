import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:flutter_webrtc/flutter_webrtc.dart';
import 'package:http/http.dart' as http;

/// Mobile implementation of platform-specific WebRTC operations.
///
/// Uses `flutter_webrtc` for direct WebRTC connections to mm-switch
/// on Android and iOS. Mirrors the web implementation's API surface.
class PlatformWebRTC {
  RTCPeerConnection? _switchPc;
  MediaStream? _switchMediaStream;
  RTCPeerConnection? _publisherPc;
  MediaStream? _publisherMediaStream;

  Timer? _viewerCountTimer;
  bool _disposed = false;

  /// Callback invoked when connection or track state changes.
  void Function()? onStateChanged;
  bool connected = false;
  bool cameraEnabled = false;
  bool micEnabled = false;

  bool get usesSwitch => _switchPc != null;
  MediaStream? get switchMediaStream => _switchMediaStream;
  MediaStream? get publisherStream => _publisherMediaStream;

  static const _iceConfig = {
    'iceServers': [
      {'urls': 'stun:stun.l.google.com:19302'},
      {
        'urls': 'turn:steegler.com:3478',
        'username': 'mm',
        'credential': 'mm_turn_secret_prod',
      },
    ]
  };

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
        '[PlatformWebRTC-mobile] connecting to mm-switch: $switchUrl source=$sourceId viewer=$id');

    final pc = await createPeerConnection(_iceConfig);
    _switchPc = pc;

    // Add recv-only transceivers for audio + video.
    await pc.addTransceiver(
      kind: RTCRtpMediaType.RTCRtpMediaTypeAudio,
      init: RTCRtpTransceiverInit(direction: TransceiverDirection.RecvOnly),
    );
    await pc.addTransceiver(
      kind: RTCRtpMediaType.RTCRtpMediaTypeVideo,
      init: RTCRtpTransceiverInit(direction: TransceiverDirection.RecvOnly),
    );

    pc.onTrack = (RTCTrackEvent event) {
      debugPrint(
          '[PlatformWebRTC-mobile] onTrack: kind=${event.track.kind}');
      if (event.streams.isNotEmpty) {
        _switchMediaStream = event.streams[0];
      }
      onStateChanged?.call();
    };

    pc.onIceConnectionState = (RTCIceConnectionState state) {
      debugPrint('[PlatformWebRTC-mobile] ICE: $state');
      connected =
          state == RTCIceConnectionState.RTCIceConnectionStateConnected ||
              state == RTCIceConnectionState.RTCIceConnectionStateCompleted;
      onStateChanged?.call();
    };

    final offer = await pc.createOffer({
      'offerToReceiveAudio': true,
      'offerToReceiveVideo': true,
    });
    await pc.setLocalDescription(offer);

    // Wait for ICE gathering.
    await Future.delayed(const Duration(milliseconds: 500));

    final localDesc = await pc.getLocalDescription();
    final headers = <String, String>{'Content-Type': 'application/json'};
    if (authToken != null && authToken.isNotEmpty) {
      headers['Authorization'] = 'Bearer $authToken';
    }
    final resp = await http.post(
      Uri.parse('$switchUrl/api/viewers/offer'),
      headers: headers,
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
      RTCSessionDescription(answer['sdp'] as String?, answer['type'] as String?),
    );
    connected = true;
    debugPrint('[PlatformWebRTC-mobile] connected to mm-switch');
    onStateChanged?.call();
  }

  /// Host-side: capture camera+mic and publish directly to mm-switch.
  Future<void> publishToSwitch(String switchUrl, String sourceId, {String? authToken}) async {
    debugPrint(
        '[PlatformWebRTC-mobile] publishing to mm-switch: $switchUrl as $sourceId');

    final stream = await navigator.mediaDevices.getUserMedia({
      'video': true,
      'audio': true,
    });
    _publisherMediaStream = stream;

    final pc = await createPeerConnection(_iceConfig);
    _publisherPc = pc;

    for (final t in stream.getTracks()) {
      await pc.addTrack(t, stream);
    }

    pc.onIceConnectionState = (RTCIceConnectionState state) {
      debugPrint('[PlatformWebRTC-mobile] publisher ICE: $state');
    };

    final offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    await Future.delayed(const Duration(milliseconds: 500));

    final localDesc = await pc.getLocalDescription();
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
      throw Exception('mm-switch publish: ${resp.statusCode} ${resp.body}');
    }
    final result = jsonDecode(resp.body) as Map<String, dynamic>;
    final answer = result['answer'] as Map<String, dynamic>;
    await pc.setRemoteDescription(
      RTCSessionDescription(answer['sdp'] as String?, answer['type'] as String?),
    );

    cameraEnabled = stream.getVideoTracks().isNotEmpty;
    micEnabled = stream.getAudioTracks().isNotEmpty;
    connected = true;
    debugPrint('[PlatformWebRTC-mobile] published to mm-switch as $sourceId');
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
    _publisherMediaStream?.getTracks().forEach((t) => t.stop());
    await _publisherPc?.close();
    _publisherPc = null;
    _publisherMediaStream = null;
    await _switchPc?.close();
    _switchPc = null;
    _switchMediaStream = null;
    cancelViewerCountPolling();
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
