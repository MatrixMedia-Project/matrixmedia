import 'dart:async';
import 'package:flutter/foundation.dart';
import 'mm_types.dart';
import 'mm_error.dart';
import 'mm_api_client.dart';
import 'mm_stream.dart';

/// Entry point for the MatrixMedia Flutter SDK.
///
/// Usage:
/// ```dart
/// final client = MMClient(serverUrl: 'http://10.0.0.105:6167');
/// await client.authenticate(openIdToken);
/// final stream = await client.joinStream(roomId: '!abc:localhost');
/// ```
class MMClient extends ChangeNotifier {
  final MMApiClient _api;
  String? _userId;
  MMStream? _activeStream;

  MMClient({required String serverUrl}) : _api = MMApiClient(baseUrl: serverUrl);

  // -----------------------------------------------------------------------
  // State
  // -----------------------------------------------------------------------

  bool get isAuthenticated => _api.isAuthenticated;
  String? get userId => _userId;
  MMStream? get activeStream => _activeStream;
  MMApiClient get api => _api;

  // -----------------------------------------------------------------------
  // Auth
  // -----------------------------------------------------------------------

  /// Authenticate with the MM server using a Matrix OpenID token.
  ///
  /// After this call, the client is ready to create/join streams.
  Future<String> authenticate(MMOpenIdToken openIdToken) async {
    final result = await _api.authenticate(openIdToken);
    _userId = result.userId;
    notifyListeners();
    return result.userId;
  }

  /// Set an existing MM JWT token (for session restoration).
  void setToken(String mmToken, {String refreshToken = '', String? userId}) {
    _api.setToken(mmToken, refreshToken);
    _userId = userId;
    notifyListeners();
  }

  /// Logout and clear tokens.
  void logout() {
    _api.clearToken();
    _userId = null;
    _activeStream?.dispose();
    _activeStream = null;
    notifyListeners();
  }

  // -----------------------------------------------------------------------
  // Stream lifecycle
  // -----------------------------------------------------------------------

  /// Create and start a new stream as host.
  Future<MMStream> startStream({
    required String roomId,
    MMStreamConfig config = const MMStreamConfig(),
  }) async {
    if (!isAuthenticated) throw MMException.notAuthenticated();

    // Create stream via API
    final info = await _api.createStream(
      roomId: roomId,
      mediaType: config.mediaType,
      title: config.title,
      e2ee: config.e2ee,
      minTier: config.minTier,
    );

    // Create MMStream
    final stream = MMStream(api: _api, info: info, isHost: true);

    // Prefer mm-switch direct publish when available — gives mm-switch full
    // PLI control over the publisher (fast keyframes, fast viewer joins).
    if (info.useSwitchPublish) {
      await stream.publishToSwitch(info.switchUrl!, info.switchSourceId!, authToken: info.switchPublisherToken);
    } else if (info.sfuUrl != null && info.sfuToken != null) {
      // Legacy fallback: publish via LiveKit
      await stream.connect(info.sfuUrl!, info.sfuToken!);
    }

    _activeStream = stream;
    notifyListeners();
    return stream;
  }

  /// Join an existing stream as viewer.
  Future<MMStream> joinStream({required String roomId}) async {
    if (!isAuthenticated) throw MMException.notAuthenticated();

    // Find active stream in room
    final streams = await _api.listRoomStreams(roomId);
    final active = streams.where((s) => s.isActive).firstOrNull;
    if (active == null) throw MMException.streamNotFound();

    // Join via API to get SFU token
    final joinResult = await _api.joinStream(active.streamId);

    // Get full stream info
    final info = await _api.getStream(active.streamId);

    // Create MMStream and connect (prefer mm-switch if available)
    final stream = MMStream(api: _api, info: info, isHost: false);
    if (joinResult.useSwitch) {
      await stream.connectViaSwitch(
        joinResult.switchUrl!,
        joinResult.switchSourceId!,
        viewerId: joinResult.switchViewerId,
        authToken: joinResult.switchViewerToken,
      );
    } else {
      await stream.connect(joinResult.sfuUrl, joinResult.sfuToken);
    }

    _activeStream = stream;
    notifyListeners();
    return stream;
  }

  /// Join a specific stream by ID.
  /// Uses mm-switch (direct Pion WebRTC with keyframe caching) when available.
  Future<MMStream> joinStreamById(String streamId) async {
    if (!isAuthenticated) throw MMException.notAuthenticated();

    final joinResult = await _api.joinStream(streamId);
    final info = await _api.getStream(streamId);

    final stream = MMStream(api: _api, info: info, isHost: false);
    if (joinResult.useSwitch) {
      await stream.connectViaSwitch(
        joinResult.switchUrl!,
        joinResult.switchSourceId!,
        viewerId: joinResult.switchViewerId,
        authToken: joinResult.switchViewerToken,
      );
    } else {
      await stream.connect(joinResult.sfuUrl, joinResult.sfuToken);
    }

    _activeStream = stream;
    notifyListeners();
    return stream;
  }

  /// Leave or end the active stream.
  Future<void> disconnect() async {
    if (_activeStream != null) {
      if (_activeStream!.isHost) {
        await _activeStream!.end();
      } else {
        await _activeStream!.leave();
      }
      _activeStream = null;
      notifyListeners();
    }
  }

  // -----------------------------------------------------------------------
  // Room queries
  // -----------------------------------------------------------------------

  /// List active streams in a Matrix room.
  Future<List<MMStreamInfo>> listRoomStreams(String roomId) =>
      _api.listRoomStreams(roomId);

  /// List recordings in a Matrix room.
  Future<List<MMRecording>> listRoomRecordings(String roomId) =>
      _api.listRoomRecordings(roomId);

  @override
  void dispose() {
    _activeStream?.dispose();
    super.dispose();
  }
}
