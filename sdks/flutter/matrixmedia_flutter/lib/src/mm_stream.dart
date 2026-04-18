import 'package:flutter/foundation.dart';
import 'package:http/http.dart' as http;
import 'package:livekit_client/livekit_client.dart';
import 'mm_types.dart';
import 'mm_api_client.dart';
import 'webrtc_platform.dart';

/// Represents an active stream connection.
///
/// Wraps a LiveKit [Room] and exposes reactive state via [ChangeNotifier].
/// Use [MMClient.joinStream] or [MMClient.startStream] to create instances.
///
/// On web, supports mm-switch direct WebRTC for low-latency source switching.
/// On mobile, falls back to LiveKit-only mode.
class MMStream extends ChangeNotifier {
  final MMApiClient _api;
  final MMStreamInfo info;
  final bool isHost;

  Room? _room;
  final PlatformWebRTC _webrtc = PlatformWebRTC();
  // mm-switch viewer-count polling (when using direct publish, LK room is
  // empty so the LK-based count is wrong; poll mm-switch instead).
  String? _switchBaseUrl;
  String? _switchSourceId;
  bool _connected = false;
  bool _disposed = false;
  int _viewerCount = 0;

  // Track state
  final List<RemoteTrackPublication> _remoteTracks = [];
  final List<LocalTrackPublication> _localTracks = [];
  bool _micEnabled = false;
  bool _cameraEnabled = false;
  bool _screenShareEnabled = false;

  MMStream({
    required MMApiClient api,
    required this.info,
    required this.isHost,
  }) : _api = api {
    _webrtc.onStateChanged = _onWebrtcStateChanged;
  }

  // -----------------------------------------------------------------------
  // Getters
  // -----------------------------------------------------------------------

  bool get connected => _connected;
  int get viewerCount => _viewerCount;
  Room? get room => _room;
  List<RemoteTrackPublication> get remoteTracks => List.unmodifiable(_remoteTracks);
  List<LocalTrackPublication> get localTracks => List.unmodifiable(_localTracks);
  bool get micEnabled => _micEnabled;
  bool get cameraEnabled => _cameraEnabled;
  bool get screenShareEnabled => _screenShareEnabled;

  String get streamId => info.streamId;
  String get title => info.title ?? 'Untitled';
  String get hostUserId => info.hostUserId;
  MMApiClient get api => _api;
  bool get usesSwitch => _webrtc.usesSwitch;

  /// The viewer's incoming MediaStream from mm-switch (web-only).
  /// Returns null on mobile.
  dynamic get switchMediaStream => _webrtc.switchMediaStream;

  /// The host's local publisher MediaStream for self-preview (web-only).
  /// Returns null on mobile.
  dynamic get publisherStream => _webrtc.publisherStream;

  // -----------------------------------------------------------------------
  // Callback from PlatformWebRTC
  // -----------------------------------------------------------------------

  void _onWebrtcStateChanged() {
    _connected = _webrtc.connected;
    _notify();
  }

  // -----------------------------------------------------------------------
  // LiveKit connection
  // -----------------------------------------------------------------------

  /// Connect to the LiveKit SFU.
  Future<void> connect(String sfuUrl, String sfuToken) async {
    // Convert http:// to ws:// for LiveKit
    final wsUrl = sfuUrl.replaceFirst('http:', 'ws:').replaceFirst('https:', 'wss:');

    _room = Room();
    _setupRoomListeners();

    await _room!.connect(wsUrl, sfuToken);
    _connected = true;
    _notify();
  }

  /// Connect via mm-switch (plain WebRTC).
  /// Forwards original VP8 RTP from the source. Per-viewer seq + ts rewriting
  /// ensures continuous timeline across source switches. See
  /// MM_SWITCH_IMPLEMENTATION_PLAN.md for the validated design.
  Future<void> connectViaSwitch(String switchUrl, String sourceId, {String? viewerId, String? authToken}) async {
    await _webrtc.connectViaSwitch(switchUrl, sourceId, viewerId: viewerId, authToken: authToken);
    _connected = _webrtc.connected;
    _switchBaseUrl = switchUrl;
    _switchSourceId = sourceId;
    _startViewerCountPolling();
    _notify();
  }

  /// Poll mm-switch for accurate viewer counts. When using direct publish,
  /// the LiveKit room is empty so the LK-based count is always 0.
  void _startViewerCountPolling() {
    if (_switchBaseUrl == null || _switchSourceId == null) return;
    _webrtc.startViewerCountPolling(
      _switchBaseUrl!,
      _switchSourceId!,
      (count) {
        if (count != _viewerCount) {
          _viewerCount = count;
          _notify();
        }
      },
    );
  }

  /// Host-side: capture camera+mic and publish directly to mm-switch as
  /// `sourceId`. Bypasses LiveKit for the streaming path so mm-switch can
  /// PLI the publisher directly (no keyframe delay) and viewers see fast
  /// source switching. LiveKit may still be connected in parallel for
  /// recording purposes -- call `connect()` separately if needed.
  Future<void> publishToSwitch(String switchUrl, String sourceId, {String? authToken}) async {
    await _webrtc.publishToSwitch(switchUrl, sourceId, authToken: authToken);
    _cameraEnabled = _webrtc.cameraEnabled;
    _micEnabled = _webrtc.micEnabled;
    _connected = _webrtc.connected;
    _switchBaseUrl = switchUrl;
    _switchSourceId = sourceId;
    _startViewerCountPolling();
    _notify();
  }

  /// Host-side: stop the mm-switch publisher.
  void stopPublishing() {
    _webrtc.stopPublishing();
    _cameraEnabled = false;
    _micEnabled = false;
    _notify();
  }

  void _setupRoomListeners() {
    final room = _room;
    if (room == null) return;

    room.addListener(_onRoomEvent);

    // Listen for track events
    room.createListener()
      ..on<TrackSubscribedEvent>((event) {
        _remoteTracks.add(event.publication);
        _notify();
      })
      ..on<TrackUnsubscribedEvent>((event) {
        _remoteTracks.remove(event.publication);
        _notify();
      })
      ..on<ParticipantConnectedEvent>((event) {
        _viewerCount = room.remoteParticipants.length + 1;
        _notify();
      })
      ..on<ParticipantDisconnectedEvent>((event) {
        _viewerCount = room.remoteParticipants.length + 1;
        _notify();
      })
      ..on<RoomDisconnectedEvent>((event) {
        _connected = false;
        _notify();
      });
  }

  void _onRoomEvent() {
    if (_disposed) return;
    _viewerCount = (_room?.remoteParticipants.length ?? 0) + 1;
  }

  // -----------------------------------------------------------------------
  // Media controls (host only)
  // -----------------------------------------------------------------------

  /// Enable/disable microphone.
  Future<void> setMicrophoneEnabled(bool enabled) async {
    if (_webrtc.publisherStream != null) {
      // mm-switch direct publish: toggle audio tracks on the MediaStream
      _webrtc.setMicEnabled(enabled);
    } else {
      await _room?.localParticipant?.setMicrophoneEnabled(enabled);
    }
    _micEnabled = enabled;
    _notify();
  }

  /// Enable/disable camera.
  Future<void> setCameraEnabled(bool enabled) async {
    if (_webrtc.publisherStream != null) {
      _webrtc.setCameraEnabled(enabled);
    } else {
      await _room?.localParticipant?.setCameraEnabled(enabled);
      if (enabled) _updateLocalTracks();
    }
    _cameraEnabled = enabled;
    _notify();
  }

  /// Enable/disable screen sharing.
  Future<void> setScreenShareEnabled(bool enabled) async {
    await _room?.localParticipant?.setScreenShareEnabled(enabled);
    _screenShareEnabled = enabled;
    if (enabled) _updateLocalTracks();
    _notify();
  }

  /// Set audio output volume (0.0 to 1.0).
  void setVolume(double volume) {
    // Volume control via LiveKit
    // LiveKit handles volume at the track level
  }

  void _updateLocalTracks() {
    _localTracks.clear();
    final local = _room?.localParticipant;
    if (local != null) {
      _localTracks.addAll(local.trackPublications.values);
    }
  }

  // -----------------------------------------------------------------------
  // Stream lifecycle
  // -----------------------------------------------------------------------

  /// Leave the stream (viewer).
  Future<void> leave() async {
    await _api.leaveStream(streamId);
    await _disconnect();
  }

  /// End the stream (host only).
  Future<void> end() async {
    await _api.endStream(streamId);
    await _disconnect();
  }

  Future<void> _disconnect() async {
    // LiveKit
    await _room?.disconnect();
    _room?.dispose();
    _room = null;
    // mm-switch (platform-specific WebRTC)
    await _webrtc.disconnect(isHost: isHost);
    // Also remove the source from mm-switch so it doesn't linger
    if (_switchBaseUrl != null && _switchSourceId != null && isHost) {
      try {
        await http.delete(
          Uri.parse('$_switchBaseUrl/api/sources/$_switchSourceId'),
          headers: {'Content-Type': 'application/json'},
        );
      } catch (_) {}
    }
    _switchBaseUrl = null;
    _switchSourceId = null;
    // State
    _connected = false;
    _cameraEnabled = false;
    _micEnabled = false;
    _remoteTracks.clear();
    _localTracks.clear();
    _notify();
  }

  void _notify() {
    if (!_disposed) notifyListeners();
  }

  @override
  void dispose() {
    _disposed = true;
    _webrtc.dispose();
    _room?.disconnect();
    _room?.dispose();
    _room = null;
    super.dispose();
  }
}
