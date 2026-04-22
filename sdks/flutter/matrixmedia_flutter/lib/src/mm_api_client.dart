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
    bool isRetry = false,
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

    final data = res.body.isEmpty
        ? <String, dynamic>{}
        : jsonDecode(res.body) as Map<String, dynamic>;

    if (res.statusCode < 400) return data;

    final ex = MMException.fromApiResponse(res.statusCode, data);

    // Auto-refresh on expired JWT. mm-core returns 403 MM_FORBIDDEN
    // with "invalid token: ExpiredSignature" once the 15-minute
    // session JWT TTL elapses. Refresh once and retry the original
    // call. Skip when:
    //   * we're already in the retry pass (no infinite loop),
    //   * we have no refresh token (login required),
    //   * the failing call IS the refresh / token endpoint itself.
    final canRetry = !isRetry &&
        (res.statusCode == 401 || res.statusCode == 403) &&
        _refreshToken != null &&
        path != '/auth/refresh' &&
        path != '/auth/token' &&
        (ex.message.contains('Expired') ||
            ex.message.contains('expired') ||
            ex.code == 'MM_TOKEN_EXPIRED');
    if (canRetry) {
      try {
        await refreshSession();
      } catch (_) {
        // Refresh failed (refresh token also expired, server rotated
        // the signing key, etc.) — surface the original error.
        throw ex;
      }
      return _request(method, path,
          body: body, extraHeaders: extraHeaders, isRetry: true);
    }

    throw ex;
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
    int? minTier,
  }) async {
    final data = await _request('POST', '/streams', body: {
      'room_id': roomId,
      'media_type': mediaType.value,
      if (title != null) 'title': title,
      'e2ee': e2ee,
      if (minTier != null) 'min_tier': minTier,
    });
    return MMStreamInfo.fromJson(data);
  }

  /// Get current creator defaults (tier gates + ads_enabled).
  Future<Map<String, dynamic>> getCreatorDefaults() async {
    return _request('GET', '/creator/me/defaults');
  }

  /// Get per-room stream-host permissions.
  /// Returns `{ mode, owner_user_id, allowed_user_ids }`.
  Future<Map<String, dynamic>> getStreamPermissions(String roomId) async {
    return _request('GET', '/rooms/${Uri.encodeComponent(roomId)}/stream-permissions');
  }

  /// Update stream-host permissions for a room (owner only).
  Future<Map<String, dynamic>> putStreamPermissions(
    String roomId, {
    required String mode,
    required List<String> allowedUserIds,
  }) async {
    return _request('PUT', '/rooms/${Uri.encodeComponent(roomId)}/stream-permissions',
        body: {
          'mode': mode,
          'allowed_user_ids': allowedUserIds,
          'owner_user_id': null,
        });
  }

  /// Claim ownership of a room's stream-host permissions (first-claim wins).
  Future<Map<String, dynamic>> claimRoomOwner(String roomId) async {
    return _request('POST',
        '/rooms/${Uri.encodeComponent(roomId)}/stream-permissions/claim');
  }

  /// Invite the MM appservice bot to a room. The bot auto-joins.
  Future<Map<String, dynamic>> enableMMInRoom(String roomId) async {
    return _request('POST', '/rooms/${Uri.encodeComponent(roomId)}/enable-mm');
  }

  /// Update creator defaults. Pass `adsEnabled: false` to opt out of ads.
  Future<Map<String, dynamic>> updateCreatorDefaults({
    int? defaultStreamMinTier,
    int? defaultRecordingMinTier,
    bool? adsEnabled,
  }) async {
    // Server requires the full object; fetch then merge.
    final cur = await getCreatorDefaults();
    return _request('PUT', '/creator/me/defaults', body: {
      'default_stream_min_tier':
          defaultStreamMinTier ?? cur['default_stream_min_tier'] ?? 0,
      'default_recording_min_tier':
          defaultRecordingMinTier ?? cur['default_recording_min_tier'] ?? 0,
      'ads_enabled': adsEnabled ?? cur['ads_enabled'] ?? true,
    });
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

  // -----------------------------------------------------------------------
  // Donations
  // -----------------------------------------------------------------------

  /// Send a donation to a stream.
  /// Create a donation. Provider: "stripe" (default, USD cents) or "lightning" (sats).
  Future<Map<String, dynamic>> donate({
    required String streamId,
    required int amountCents,
    String? message,
    String provider = 'stripe',
  }) async {
    return _request('POST', '/donations', body: {
      'stream_id': streamId,
      'amount_cents': amountCents,
      if (message != null && message.isNotEmpty) 'message': message,
      'provider': provider,
    });
  }

  /// Create a Lightning donation (amount in sats).
  /// Returns bolt11 invoice in checkout_url field.
  Future<Map<String, dynamic>> donateLightning({
    required String streamId,
    required int amountSats,
    String? message,
  }) async {
    return _request('POST', '/donations', body: {
      'stream_id': streamId,
      'amount_cents': amountSats, // For Lightning, cents field carries sats
      'provider': 'lightning',
      if (message != null && message.isNotEmpty) 'message': message,
    });
  }

  /// Check Lightning payment status.
  Future<bool> checkLightningPayment(String paymentHash) async {
    final data = await _request('GET', '/payments/lightning/$paymentHash');
    return data['paid'] == true;
  }

  /// Get donation feed for a stream.
  Future<List<Map<String, dynamic>>> getDonationFeed(String streamId) async {
    final data = await _request('GET', '/streams/$streamId/donations');
    return (data['donations'] as List<dynamic>? ?? [])
        .cast<Map<String, dynamic>>();
  }

  // -----------------------------------------------------------------------
  // Subscriptions & Tiers
  // -----------------------------------------------------------------------

  /// List tiers for a creator.
  Future<List<Map<String, dynamic>>> listCreatorTiers(String creatorUserId) async {
    final data = await _request('GET', '/creators/${Uri.encodeComponent(creatorUserId)}/tiers');
    return (data['tiers'] as List<dynamic>? ?? [])
        .cast<Map<String, dynamic>>();
  }

  /// Subscribe to a tier.
  Future<Map<String, dynamic>> subscribe(String tierId) async {
    return _request('POST', '/subscriptions', body: {'tier_id': tierId});
  }

  /// Check entitlement for a creator.
  Future<Map<String, dynamic>> checkEntitlement(String creatorUserId) async {
    return _request('GET', '/subscriptions/check?creator_user_id=${Uri.encodeComponent(creatorUserId)}');
  }

  /// Create a subscription tier.
  Future<Map<String, dynamic>> createTier({
    required String name,
    required int tierLevel,
    required int priceCents,
    List<String> perks = const [],
  }) async {
    return _request('POST', '/creator/tiers', body: {
      'name': name,
      'tier_level': tierLevel,
      'price_cents': priceCents,
      'perks': perks,
    });
  }

  /// Onboard as creator.
  Future<Map<String, dynamic>> onboardCreator(String displayName) async {
    return _request('POST', '/creator/onboard', body: {
      'display_name': displayName,
    });
  }

  /// Get creator profile.
  Future<Map<String, dynamic>?> getCreatorProfile() async {
    try {
      return await _request('GET', '/creator/profile');
    } catch (e) {
      if (e is MMException && (e.httpStatus == 404 || e.httpStatus == 412)) return null;
      rethrow;
    }
  }

  // -----------------------------------------------------------------------
  // Discovery
  // -----------------------------------------------------------------------

  /// Get trending streams.
  Future<Map<String, dynamic>> discoverTrending() async {
    return _request('GET', '/discover/trending');
  }

  /// Get categories.
  Future<Map<String, dynamic>> discoverCategories() async {
    return _request('GET', '/discover/categories');
  }

  /// Get creators list.
  Future<Map<String, dynamic>> discoverCreators() async {
    return _request('GET', '/discover/creators');
  }

  /// Follow a creator.
  Future<void> followCreator(String creatorUserId) async {
    await _request('POST', '/discover/follow/${Uri.encodeComponent(creatorUserId)}');
  }

  /// Get following list.
  Future<Map<String, dynamic>> discoverFollowing() async {
    return _request('GET', '/discover/following');
  }

  /// List user's subscriptions.
  Future<List<Map<String, dynamic>>> listSubscriptions() async {
    final data = await _request('GET', '/subscriptions');
    return (data['subscriptions'] as List<dynamic>? ?? [])
        .cast<Map<String, dynamic>>();
  }

  // -----------------------------------------------------------------------
  // Content Gates
  // -----------------------------------------------------------------------

  /// Create a content gate.
  Future<Map<String, dynamic>> createGate({
    required String contentType,
    required String contentId,
    required int minTierLevel,
    int previewSeconds = 120,
  }) async {
    return _request('POST', '/gates', body: {
      'content_type': contentType,
      'content_id': contentId,
      'min_tier_level': minTierLevel,
      'preview_seconds': previewSeconds,
    });
  }

  /// Get gate for content.
  Future<Map<String, dynamic>?> getGate(String contentType, String contentId) async {
    final data = await _request('GET', '/gates/${Uri.encodeComponent(contentType)}/${Uri.encodeComponent(contentId)}');
    if (data['gate'] == null && data['id'] == null) return null;
    return data;
  }

  /// Delete gate.
  Future<void> deleteGate(String contentType, String contentId) async {
    await _request('DELETE', '/gates/${Uri.encodeComponent(contentType)}/${Uri.encodeComponent(contentId)}');
  }

  // -----------------------------------------------------------------------
  // Recording
  // -----------------------------------------------------------------------

  /// Start server-side recording of a stream.
  Future<Map<String, dynamic>> startRecording(String streamId) async {
    return _request('POST', '/streams/$streamId/record');
  }

  /// Stop server-side recording of a stream.
  Future<Map<String, dynamic>> stopRecording(String streamId) async {
    return _request('DELETE', '/streams/$streamId/record');
  }

  // -----------------------------------------------------------------------
  // Advertising (Phase 9)
  // -----------------------------------------------------------------------

  /// Get an ad decision for a stream (pre-roll, mid-roll, etc.).
  Future<MMAdDecision> getAdDecision(String streamId, {String slot = 'pre_roll'}) async {
    final data = await _request('GET', '/streams/$streamId/ad-decision?slot=$slot');
    return MMAdDecision.fromJson(data);
  }

  /// Submit ad completion proof (HMAC challenge-response).
  Future<void> submitAdComplete(String streamId, {
    required String impressionToken,
    required String challengeResponse,
    required int timestamp,
  }) async {
    await _request('POST', '/streams/$streamId/ad-complete', body: {
      'impression_token': impressionToken,
      'challenge_response': challengeResponse,
      'timestamp': timestamp,
    });
  }

  /// Report an ad event (quartile progress, click, skip, error).
  Future<void> reportAdEvent({
    required String impressionToken,
    required String event,
    int? positionSecs,
  }) async {
    await _request('POST', '/ads/events', body: {
      'impression_token': impressionToken,
      'event': event,
      if (positionSecs != null) 'position_secs': positionSecs,
    });
  }

  /// Check if the viewer is currently in an ad break.
  Future<bool> getAdStatus(String streamId) async {
    final data = await _request('GET', '/streams/$streamId/ad-status');
    return data['in_ad_break'] == true;
  }

  /// Upload an ad creative.
  Future<MMAdCreative> uploadAd({
    required String title,
    required String placement,
    int durationSecs = 15,
    String? clickThroughUrl,
    List<String> categories = const [],
  }) async {
    final data = await _request('POST', '/ads', body: {
      'title': title,
      'placement': placement,
      'duration_secs': durationSecs,
      if (clickThroughUrl != null) 'click_through_url': clickThroughUrl,
      'categories': categories,
    });
    return MMAdCreative.fromJson(data);
  }

  /// List my ads.
  Future<List<MMAdCreative>> listMyAds() async {
    final data = await _request('GET', '/ads');
    return (data['ads'] as List<dynamic>? ?? [])
        .map((e) => MMAdCreative.fromJson(e as Map<String, dynamic>))
        .toList();
  }

  /// Delete an ad.
  Future<void> deleteAd(String adId) async {
    await _request('DELETE', '/ads/$adId');
  }

  /// Get ad statistics.
  Future<MMAdStats> getAdStats(String adId) async {
    final data = await _request('GET', '/ads/$adId/stats');
    return MMAdStats.fromJson(data);
  }

  /// Trigger mid-roll ad break (host only).
  Future<void> triggerAdBreak(String streamId) async {
    await _request('POST', '/streams/$streamId/ad-break');
  }
}
