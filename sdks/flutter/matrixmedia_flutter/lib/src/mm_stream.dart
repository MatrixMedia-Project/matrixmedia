import 'dart:async';
import 'package:flutter/foundation.dart';
import 'package:livekit_client/livekit_client.dart';
import 'mm_types.dart';
import 'mm_api_client.dart';

/// Represents an active stream connection.
///
/// Wraps a LiveKit [Room] and exposes reactive state via [ChangeNotifier].
/// Use [MMClient.joinStream] or [MMClient.startStream] to create instances.
class MMStream extends ChangeNotifier {
  final MMApiClient _api;
  final MMStreamInfo info;
  final bool isHost;

  Room? _room;
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
  }) : _api = api;

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
    await _room?.localParticipant?.setMicrophoneEnabled(enabled);
    _micEnabled = enabled;
    _notify();
  }

  /// Enable/disable camera.
  Future<void> setCameraEnabled(bool enabled) async {
    await _room?.localParticipant?.setCameraEnabled(enabled);
    _cameraEnabled = enabled;
    if (enabled) {
      _updateLocalTracks();
    }
    _notify();
  }

  /// Enable/disable screen sharing.
  Future<void> setScreenShareEnabled(bool enabled) async {
    await _room?.localParticipant?.setScreenShareEnabled(enabled);
    _screenShareEnabled = enabled;
    if (enabled) {
      _updateLocalTracks();
    }
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
    await _room?.disconnect();
    _room?.dispose();
    _room = null;
    _connected = false;
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
    _room?.disconnect();
    _room?.dispose();
    _room = null;
    super.dispose();
  }
}
