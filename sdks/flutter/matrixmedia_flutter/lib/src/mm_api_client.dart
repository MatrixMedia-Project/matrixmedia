import 'dart:convert';
import 'package:http/http.dart' as http;
import 'mm_types.dart';
import 'mm_error.dart';

/// HTTP client for the MatrixMedia backend API (/_mm/client/v1/).
class MMApiClient {
  final String baseUrl;
  String? _mmToken;
  String? _refreshToken;

  MMApiClient({required this.baseUrl});

  /// Set the current MM JWT token.
  void setToken(String token, String refreshToken) {
    _mmToken = token;
    _refreshToken = refreshToken;
  }

  /// Clear tokens (logout).
  void clearToken() {
    _mmToken = null;
    _refreshToken = null;
  }

  bool get isAuthenticated => _mmToken != null;

  // -----------------------------------------------------------------------
  // HTTP helpers
  // -----------------------------------------------------------------------

  Map<String, String> get _headers => {
        'Content-Type': 'application/json',
        if (_mmToken != null) 'Authorization': 'Bearer $_mmToken',
      };

  Future<Map<String, dynamic>> _request(
    String method,
    String path, {
    Map<String, dynamic>? body,
    Map<String, String>? extraHeaders,
  }) async {
    final uri = Uri.parse('$baseUrl/_mm/client/v1$path');
    final headers = {..._headers, ...?extraHeaders};

    late http.Response res;
    switch (method) {
      case 'GET':
        res = await http.get(uri, headers: headers);
        break;
      case 'POST':
        res = await http.post(uri, headers: headers, body: body != null ? jsonEncode(body) : null);
        break;
      case 'PUT':
        res = await http.put(uri, headers: headers, body: body != null ? jsonEncode(body) : null);
        break;
      case 'DELETE':
        res = await http.delete(uri, headers: headers);
        break;
      default:
        throw MMException.network('Unknown method: $method');
    }

    if (res.body.isEmpty) return {};
    final data = jsonDecode(res.body) as Map<String, dynamic>;

    if (res.statusCode >= 400) {
      throw MMException.fromApiResponse(res.statusCode, data);
    }

    return data;
  }

  // -----------------------------------------------------------------------
  // Auth
  // -----------------------------------------------------------------------

  /// Exchange a Matrix OpenID token for an MM JWT.
  Future<MMAuthResult> authenticate(MMOpenIdToken openIdToken) async {
    final data = await _request('POST', '/auth/token', body: {
      'openid_token': openIdToken.toJson(),
    });
    final result = MMAuthResult.fromJson(data);
    setToken(result.mmToken, result.refreshToken);
    return result;
  }

  /// Refresh the MM JWT using the refresh token.
  Future<MMAuthResult> refreshSession() async {
    if (_refreshToken == null) throw MMException.notAuthenticated();
    final data = await _request('POST', '/auth/refresh', body: {
      'refresh_token': _refreshToken,
    });
    final result = MMAuthResult.fromJson(data);
    setToken(result.mmToken, result.refreshToken);
    return result;
  }

  // -----------------------------------------------------------------------
  // Streams
  // -----------------------------------------------------------------------

  /// Create a new stream in a Matrix room.
  Future<MMStreamInfo> createStream({
    required String roomId,
    MMMediaType mediaType = MMMediaType.audio,
    String? title,
    bool e2ee = false,
  }) async {
    final data = await _request('POST', '/streams', body: {
      'room_id': roomId,
      'media_type': mediaType.value,
      if (title != null) 'title': title,
      'e2ee': e2ee,
    });
    return MMStreamInfo.fromJson(data);
  }

  /// Get stream details.
  Future<MMStreamInfo> getStream(String streamId) async {
    final data = await _request('GET', '/streams/$streamId');
    return MMStreamInfo.fromJson(data);
  }

  /// Join a stream as viewer.
  Future<MMJoinResult> joinStream(String streamId) async {
    final data = await _request('POST', '/streams/$streamId/join',
        extraHeaders: {'Idempotency-Key': DateTime.now().microsecondsSinceEpoch.toString()});
    return MMJoinResult.fromJson(data);
  }

  /// Leave a stream.
  Future<void> leaveStream(String streamId) async {
    await _request('POST', '/streams/$streamId/leave');
  }

  /// End a stream (host only).
  Future<void> endStream(String streamId) async {
    await _request('POST', '/streams/$streamId/end',
        extraHeaders: {'Idempotency-Key': DateTime.now().microsecondsSinceEpoch.toString()});
  }

  /// List active streams in a Matrix room.
  Future<List<MMStreamInfo>> listRoomStreams(String roomId) async {
    try {
      final data = await _request('GET', '/rooms/${Uri.encodeComponent(roomId)}/streams');
      final list = data['streams'] as List<dynamic>? ?? [];
      return list.map((e) => MMStreamInfo.fromJson(e as Map<String, dynamic>)).toList();
    } catch (e) {
      if (e is MMException && e.httpStatus == 404) return [];
      rethrow;
    }
  }

  /// List participants in a stream.
  Future<List<MMParticipant>> listParticipants(String streamId) async {
    final data = await _request('GET', '/streams/$streamId/participants');
    final list = data['participants'] as List<dynamic>? ?? [];
    return list.map((e) => MMParticipant.fromJson(e as Map<String, dynamic>)).toList();
  }

  /// Rotate E2EE key (host only).
  Future<void> rotateKey(String streamId) async {
    await _request('POST', '/streams/$streamId/rotate-key');
  }

  // -----------------------------------------------------------------------
  // Recordings
  // -----------------------------------------------------------------------

  /// List recordings in a room.
  Future<List<MMRecording>> listRoomRecordings(String roomId, {int limit = 20}) async {
    final data = await _request('GET', '/rooms/${Uri.encodeComponent(roomId)}/recordings?limit=$limit');
    final list = data['recordings'] as List<dynamic>? ?? [];
    return list.map((e) => MMRecording.fromJson(e as Map<String, dynamic>)).toList();
  }

  /// Get recording details.
  Future<MMRecording> getRecording(String recordingId) async {
    final data = await _request('GET', '/recordings/$recordingId');
    return MMRecording.fromJson(data);
  }
}
