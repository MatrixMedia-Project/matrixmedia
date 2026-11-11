import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:flutter_webrtc/flutter_webrtc.dart';
import 'package:http/http.dart' as http;

import 'publish_watchdog.dart';
import 'turn_config.dart';

/// Mobile implementation of platform-specific WebRTC operations.
///
/// Uses `flutter_webrtc` for direct WebRTC connections to mm-switch
/// on Android and iOS. Mirrors the web implementation's API surface.
class PlatformWebRTC {
  RTCPeerConnection? _switchPc;
  MediaStream? _switchMediaStream;
  RTCPeerConnection? _publisherPc;
  MediaStream? _publisherMediaStream;
  Timer? _watchdogTimer;
  bool _watchdogTickRunning = false;
  final PublishWatchdog _watchdog = PublishWatchdog();

  /// Callback invoked when connection or track state changes.
  void Function()? onStateChanged;
  bool connected = false;
  bool cameraEnabled = false;
  bool micEnabled = false;

  /// True when the publisher watchdog gave up after [PublishWatchdog.maxRestarts]
  /// dead-uplink restarts. The host UI should surface this (check uplink/VPN)
  /// instead of showing a silently black stream.
  bool publisherFailed = false;

  bool get usesSwitch => _switchPc != null;
  MediaStream? get switchMediaStream => _switchMediaStream;
  MediaStream? get publisherStream => _publisherMediaStream;

  static const _iceConfig = {'iceServers': turnIceServers};

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
      '[PlatformWebRTC-mobile] connecting to mm-switch: $switchUrl source=$sourceId viewer=$id',
    );

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
      debugPrint('[PlatformWebRTC-mobile] onTrack: kind=${event.track.kind}');
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
      RTCSessionDescription(
        answer['sdp'] as String?,
        answer['type'] as String?,
      ),
    );
    connected = true;
    debugPrint('[PlatformWebRTC-mobile] connected to mm-switch');
    onStateChanged?.call();
  }

  /// Host-side: capture camera+mic and publish directly to mm-switch.
  Future<void> publishToSwitch(
    String switchUrl,
    String sourceId, {
    String? authToken,
  }) async {
    debugPrint(
      '[PlatformWebRTC-mobile] publishing to mm-switch: $switchUrl as $sourceId',
    );

    final stream = await navigator.mediaDevices.getUserMedia({
      'video': true,
      'audio': true,
    });
    _publisherMediaStream = stream;
    publisherFailed = false;

    await _connectPublisherPc(switchUrl, sourceId, authToken: authToken);

    cameraEnabled = stream.getVideoTracks().isNotEmpty;
    micEnabled = stream.getAudioTracks().isNotEmpty;
    connected = true;
    debugPrint('[PlatformWebRTC-mobile] published to mm-switch as $sourceId');
    _startPublisherWatchdog(switchUrl, sourceId, authToken);
    onStateChanged?.call();
  }

  /// Creates a fresh publisher peer connection over the already-captured
  /// [_publisherMediaStream] tracks and negotiates it with mm-switch as
  /// `sourceId`.
  Future<void> _connectPublisherPc(
    String switchUrl,
    String sourceId, {
    String? authToken,
  }) async {
    final stream = _publisherMediaStream;
    if (stream == null) {
      throw StateError('publish requested with no captured media stream');
    }
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
      RTCSessionDescription(
        answer['sdp'] as String?,
        answer['type'] as String?,
      ),
    );
  }

  /// Dead-uplink watchdog: samples `outbound-rtp bytesSent` every 2.5s;
  /// ~10s of zero growth means RTP is blackholed, so the peer connection
  /// is closed and re-published under the same stream id.
  void _startPublisherWatchdog(
    String switchUrl,
    String sourceId,
    String? authToken,
  ) {
    _cancelPublisherWatchdog();
    _watchdog.reset();
    _watchdogTimer = Timer.periodic(const Duration(milliseconds: 2500), (_) {
      _watchdogTick(switchUrl, sourceId, authToken);
    });
  }

  void _cancelPublisherWatchdog() {
    _watchdogTimer?.cancel();
    _watchdogTimer = null;
  }

  Future<void> _watchdogTick(
    String switchUrl,
    String sourceId,
    String? authToken,
  ) async {
    final pc = _publisherPc;
    if (pc == null || _watchdogTickRunning) return;
    _watchdogTickRunning = true;
    try {
      final stats = await pc.getStats();
      final bytes = sumOutboundBytesSent(stats.map((s) => s.values).toList());
      switch (_watchdog.sample(bytes)) {
        case WatchdogAction.ok:
          break;
        case WatchdogAction.restart:
          debugPrint(
            '[PlatformWebRTC-mobile] publisher stalled '
            '(outbound bytesSent=$bytes, no growth ~10s): re-publishing '
            '$sourceId (attempt ${_watchdog.restarts}/${_watchdog.maxRestarts})',
          );
          _publisherPc?.close();
          _publisherPc = null;
          try {
            await _connectPublisherPc(
              switchUrl,
              sourceId,
              authToken: authToken,
            );
            connected = true;
          } catch (e) {
            debugPrint('[PlatformWebRTC-mobile] publisher restart failed: $e');
          }
          onStateChanged?.call();
          break;
        case WatchdogAction.failed:
          _cancelPublisherWatchdog();
          publisherFailed = true;
          debugPrint(
            '[PlatformWebRTC-mobile] publisher still stalled after '
            '${_watchdog.maxRestarts} restarts (bytesSent=$bytes): giving '
            'up — check uplink/VPN/firewall, then rejoin',
          );
          onStateChanged?.call();
          break;
      }
    } catch (e) {
      debugPrint('[PlatformWebRTC-mobile] publisher watchdog tick failed: $e');
    } finally {
      _watchdogTickRunning = false;
    }
  }

  /// Host-side: stop the mm-switch publisher.
  void stopPublishing() {
    _cancelPublisherWatchdog();
    _watchdog.reset();
    publisherFailed = false;
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

  /// Disconnect and clean up all WebRTC resources.
  Future<void> disconnect({bool isHost = false}) async {
    _cancelPublisherWatchdog();
    _watchdog.reset();
    publisherFailed = false;
    _publisherMediaStream?.getTracks().forEach((t) => t.stop());
    await _publisherPc?.close();
    _publisherPc = null;
    _publisherMediaStream = null;
    await _switchPc?.close();
    _switchPc = null;
    _switchMediaStream = null;
    connected = false;
    cameraEnabled = false;
    micEnabled = false;
  }

  void dispose() {
    _cancelPublisherWatchdog();
    _switchPc?.close();
    _switchPc = null;
    _publisherMediaStream?.getTracks().forEach((t) => t.stop());
    _publisherPc?.close();
    _publisherPc = null;
    _publisherMediaStream = null;
  }
}
