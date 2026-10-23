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
  /// Wait until ICE gathering completes (or [timeout] elapses) before the
  /// offer is read. A fixed short delay is not enough: STUN/TURN lookups
  /// routinely take longer than a few hundred milliseconds, and an offer
  /// sent without srflx/relay candidates leaves the switch with only the
  /// browser's (unreachable) host candidates — publish and view both fail
  /// silently with zero RTP flowing.
  ///
  /// `dart:html` exposes no gathering-state-change stream, so completion is
  /// detected two ways: the null candidate at the end of [onIceCandidate],
  /// plus polling [iceGatheringState] in case gathering already finished.
  static Future<void> waitIceGatheringComplete(
    html.RtcPeerConnection pc, {
    Duration timeout = const Duration(seconds: 8),
  }) async {
    if (pc.iceGatheringState == 'complete') return;
    final completer = Completer<void>();
    late StreamSubscription<html.RtcPeerConnectionIceEvent> sub;
    Timer? timer;
    Timer? poller;
    void done() {
      if (completer.isCompleted) return;
      completer.complete();
      timer?.cancel();
      poller?.cancel();
      sub.cancel();
    }

    sub = pc.onIceCandidate.listen((event) {
      if (event.candidate == null) done();
    });
    poller = Timer.periodic(const Duration(milliseconds: 200), (_) {
      if (pc.iceGatheringState == 'complete') done();
    });
    timer = Timer(timeout, done);
    await completer.future;
  }

  html.RtcPeerConnection? _switchPc;
  html.MediaStream? _switchMediaStream;
  html.RtcPeerConnection? _publisherPc;
  html.MediaStream? _publisherMediaStream;

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
    await waitIceGatheringComplete(pc);

    final localDesc = pc.localDescription;
    debugPrint('[PlatformWebRTC] viewer offer SDP:\n${localDesc?.sdp}');
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
      debugPrint('[PlatformWebRTC] offer rejected ${resp.statusCode}: ${resp.body}');
      throw Exception('mm-switch offer: ${resp.statusCode} ${resp.body}');
    }

    final result = jsonDecode(resp.body) as Map<String, dynamic>;
    final answer = result['answer'] as Map<String, dynamic>;
    final answerSdp = '${answer['sdp']}';
    debugPrint('[PlatformWebRTC] answer m-lines: '
        '${RegExp(r'^m=', multiLine: true).allMatches(answerSdp).length}');
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
    await waitIceGatheringComplete(pc);

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
    // State
    connected = false;
    cameraEnabled = false;
    micEnabled = false;
  }

  void dispose() {
    _switchPc?.close();
    _switchPc = null;
    _publisherMediaStream?.getTracks().forEach((t) => t.stop());
    _publisherPc?.close();
    _publisherPc = null;
    _publisherMediaStream = null;
  }
}
