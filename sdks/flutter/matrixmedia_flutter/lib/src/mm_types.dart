/// MatrixMedia model types.
///
/// Mirrors the Swift/Kotlin SDK API shapes for cross-platform consistency.

/// OpenID token from a Matrix homeserver.
class MMOpenIdToken {
  final String accessToken;
  final String tokenType;
  final String matrixServerName;
  final int expiresIn;

  const MMOpenIdToken({
    required this.accessToken,
    this.tokenType = 'Bearer',
    required this.matrixServerName,
    required this.expiresIn,
  });

  Map<String, dynamic> toJson() => {
        'access_token': accessToken,
        'token_type': tokenType,
        'matrix_server_name': matrixServerName,
        'expires_in': expiresIn,
      };
}

/// Authenticated MM session info.
class MMAuthResult {
  final String mmToken;
  final String refreshToken;
  final String userId;
  final int expiresIn;

  const MMAuthResult({
    required this.mmToken,
    required this.refreshToken,
    required this.userId,
    required this.expiresIn,
  });

  factory MMAuthResult.fromJson(Map<String, dynamic> json) => MMAuthResult(
        mmToken: json['mm_token'] as String,
        refreshToken: json['refresh_token'] as String? ?? '',
        userId: json['user_id'] as String,
        expiresIn: json['expires_in'] as int? ?? 900,
      );
}

/// Stream media type.
enum MMMediaType {
  audio,
  video,
  screen;

  String get value => name;

  static MMMediaType fromString(String s) {
    switch (s) {
      case 'video':
        return MMMediaType.video;
      case 'screen':
        return MMMediaType.screen;
      default:
        return MMMediaType.audio;
    }
  }
}

/// Stream status.
enum MMStreamStatus {
  active,
  ending,
  ended,
  failed;

  static MMStreamStatus fromString(String s) {
    switch (s) {
      case 'ending':
        return MMStreamStatus.ending;
      case 'ended':
        return MMStreamStatus.ended;
      case 'failed':
        return MMStreamStatus.failed;
      default:
        return MMStreamStatus.active;
    }
  }
}

/// Stream info from the MM API.
class MMStreamInfo {
  final String streamId;
  final String roomId;
  final String hostUserId;
  final MMMediaType mediaType;
  final String? title;
  final MMStreamStatus status;
  final int participantCount;
  final DateTime startedAt;
  final DateTime? endedAt;
  final String? sfuUrl;
  final String? sfuToken;
  final MME2eeInfo? e2ee;
  /// mm-switch URL — set on stream creation when host should publish directly.
  final String? switchUrl;
  /// Source id host should publish as (e.g. `stream-{id}`).
  final String? switchSourceId;
  /// HMAC token for mm-switch publisher authentication.
  final String? switchPublisherToken;

  const MMStreamInfo({
    required this.streamId,
    required this.roomId,
    required this.hostUserId,
    required this.mediaType,
    this.title,
    required this.status,
    required this.participantCount,
    required this.startedAt,
    this.endedAt,
    this.sfuUrl,
    this.sfuToken,
    this.e2ee,
    this.switchUrl,
    this.switchSourceId,
    this.switchPublisherToken,
  });

  bool get isActive => status == MMStreamStatus.active;
  bool get useSwitchPublish => switchUrl != null && switchUrl!.isNotEmpty
      && switchSourceId != null && switchSourceId!.isNotEmpty;

  factory MMStreamInfo.fromJson(Map<String, dynamic> json) => MMStreamInfo(
        streamId: (json['stream_id'] ?? json['id'] ?? '') as String,
        roomId: (json['room_id'] ?? '').toString(),
        hostUserId: (json['host_user_id'] ?? json['host'] ?? '') as String,
        mediaType: MMMediaType.fromString(json['media_type'] as String? ?? 'audio'),
        title: json['title'] as String?,
        status: MMStreamStatus.fromString(json['status'] as String? ?? 'active'),
        participantCount: json['participant_count'] as int? ?? 0,
        startedAt: DateTime.tryParse(json['started_at'] as String? ?? '') ?? DateTime.now(),
        endedAt: json['ended_at'] != null ? DateTime.tryParse(json['ended_at'] as String) : null,
        sfuUrl: json['sfu_url'] as String?,
        sfuToken: json['sfu_token'] as String?,
        e2ee: json['e2ee'] != null ? MME2eeInfo.fromJson(json['e2ee'] as Map<String, dynamic>) : null,
        switchUrl: json['switch_url'] as String?,
        switchSourceId: json['switch_source_id'] as String?,
        switchPublisherToken: json['switch_publisher_token'] as String?,
      );
}

/// Join stream response.
class MMJoinResult {
  final String sfuUrl;
  final String sfuToken;
  final String participantId;
  final MME2eeInfo? e2ee;
  final String? switchUrl;
  final String? switchSourceId;
  /// Server-assigned viewer id. MUST be used verbatim as `id` when posting
  /// to `/api/viewers/offer` — server-side ad switching uses the same id.
  final String? switchViewerId;
  /// HMAC token for mm-switch viewer authentication.
  final String? switchViewerToken;

  const MMJoinResult({
    required this.sfuUrl,
    required this.sfuToken,
    required this.participantId,
    this.e2ee,
    this.switchUrl,
    this.switchSourceId,
    this.switchViewerId,
    this.switchViewerToken,
  });

  bool get useSwitch => switchUrl != null && switchUrl!.isNotEmpty;

  factory MMJoinResult.fromJson(Map<String, dynamic> json) => MMJoinResult(
        sfuUrl: json['sfu_url'] as String? ?? '',
        sfuToken: json['sfu_token'] as String? ?? '',
        participantId: json['participant_id'] as String? ?? '',
        e2ee: json['e2ee'] != null ? MME2eeInfo.fromJson(json['e2ee'] as Map<String, dynamic>) : null,
        switchUrl: json['switch_url'] as String?,
        switchSourceId: json['switch_source_id'] as String?,
        switchViewerId: json['switch_viewer_id'] as String?,
        switchViewerToken: json['switch_viewer_token'] as String?,
      );
}

/// E2EE info for a stream.
class MME2eeInfo {
  final bool enabled;
  final String algorithm;
  final String keyId;
  final int keyGeneration;
  final String keyB64;

  const MME2eeInfo({
    required this.enabled,
    required this.algorithm,
    required this.keyId,
    required this.keyGeneration,
    required this.keyB64,
  });

  factory MME2eeInfo.fromJson(Map<String, dynamic> json) => MME2eeInfo(
        enabled: json['enabled'] as bool? ?? false,
        algorithm: json['algorithm'] as String? ?? 'aes-gcm-256',
        keyId: json['key_id'] as String? ?? '',
        keyGeneration: json['key_generation'] as int? ?? 0,
        keyB64: json['key_b64'] as String? ?? '',
      );
}

/// Participant info.
class MMParticipant {
  final String id;
  final String userId;
  final String? displayName;
  final String role;

  const MMParticipant({
    required this.id,
    required this.userId,
    this.displayName,
    required this.role,
  });

  bool get isHost => role == 'host';

  factory MMParticipant.fromJson(Map<String, dynamic> json) => MMParticipant(
        id: json['id'] as String? ?? '',
        userId: json['user_id'] as String? ?? '',
        displayName: json['display_name'] as String?,
        role: json['role'] as String? ?? 'viewer',
      );
}

/// Recording info.
class MMRecording {
  final String id;
  final String streamId;
  final String hostUserId;
  final MMMediaType mediaType;
  final String? title;
  final String status;
  final int? durationMs;
  final int? sizeBytes;
  final String? playbackUrl;
  final String? mxcUrl;
  final DateTime createdAt;
  /// Ad policy for VoD playback (pre-roll, mid-rolls, post-roll).
  /// Null when advertising is disabled or viewer has ad-free perk.
  final MMAdPolicy? adPolicy;

  const MMRecording({
    required this.id,
    required this.streamId,
    required this.hostUserId,
    required this.mediaType,
    this.title,
    required this.status,
    this.durationMs,
    this.sizeBytes,
    this.playbackUrl,
    this.mxcUrl,
    required this.createdAt,
    this.adPolicy,
  });

  factory MMRecording.fromJson(Map<String, dynamic> json) => MMRecording(
        id: json['id'] as String? ?? '',
        streamId: json['stream_id'] as String? ?? '',
        hostUserId: json['host_user_id'] as String? ?? '',
        mediaType: MMMediaType.fromString(json['media_type'] as String? ?? 'audio'),
        title: json['title'] as String?,
        status: json['status'] as String? ?? 'ready',
        durationMs: json['duration_ms'] as int?,
        sizeBytes: json['size_bytes'] as int?,
        playbackUrl: json['playback_url'] as String?,
        mxcUrl: json['mxc_url'] as String?,
        createdAt: DateTime.tryParse(json['created_at'] as String? ?? '') ?? DateTime.now(),
        adPolicy: json['ad_policy'] != null
            ? MMAdPolicy.fromJson(json['ad_policy'] as Map<String, dynamic>)
            : null,
      );
}

// ---------------------------------------------------------------------------
// Advertising types (Phase 9)
// ---------------------------------------------------------------------------

/// Ad decision returned by the server.
class MMAdDecision {
  final String type; // "serve_ad" or "no_ad"
  final MMAdDecisionAd? ad;
  final String? impressionToken;
  final String? challenge;
  final String? viewerSecret;
  final String? slot;
  final String? enforcement;
  final int? skipAfterSecs;
  final String? reason; // for no_ad

  const MMAdDecision({
    required this.type,
    this.ad,
    this.impressionToken,
    this.challenge,
    this.viewerSecret,
    this.slot,
    this.enforcement,
    this.skipAfterSecs,
    this.reason,
  });

  bool get hasAd => type == 'serve_ad' && ad != null;

  factory MMAdDecision.fromJson(Map<String, dynamic> json) {
    final type = json['type'] as String? ?? 'no_ad';
    return MMAdDecision(
      type: type,
      ad: json['ad'] != null ? MMAdDecisionAd.fromJson(json['ad'] as Map<String, dynamic>) : null,
      impressionToken: json['impression_token'] as String?,
      challenge: json['challenge'] as String?,
      viewerSecret: json['viewer_secret'] as String?,
      slot: json['slot'] as String?,
      enforcement: json['enforcement'] as String?,
      skipAfterSecs: json['skip_after_secs'] as int?,
      reason: json['reason'] as String?,
    );
  }
}

/// Ad creative metadata in a decision.
class MMAdDecisionAd {
  final String adId;
  final String title;
  final String mediaUrl;
  final int durationSecs;
  final String? clickThroughUrl;
  final String ownerType;

  const MMAdDecisionAd({
    required this.adId,
    required this.title,
    required this.mediaUrl,
    required this.durationSecs,
    this.clickThroughUrl,
    required this.ownerType,
  });

  factory MMAdDecisionAd.fromJson(Map<String, dynamic> json) => MMAdDecisionAd(
    adId: json['ad_id'] as String? ?? '',
    title: json['title'] as String? ?? '',
    mediaUrl: json['media_url'] as String? ?? '',
    durationSecs: json['duration_secs'] as int? ?? 15,
    clickThroughUrl: json['click_through_url'] as String?,
    ownerType: json['owner_type'] as String? ?? 'creator',
  );
}

/// Ad creative (uploaded by creator or platform).
class MMAdCreative {
  final String id;
  final String title;
  final String placement;
  final int durationSecs;
  final String status;
  final String ownerType;
  final List<String> categories;
  final DateTime createdAt;

  const MMAdCreative({
    required this.id,
    required this.title,
    required this.placement,
    required this.durationSecs,
    required this.status,
    required this.ownerType,
    this.categories = const [],
    required this.createdAt,
  });

  factory MMAdCreative.fromJson(Map<String, dynamic> json) => MMAdCreative(
    id: json['id'] as String? ?? '',
    title: json['title'] as String? ?? '',
    placement: json['placement'] as String? ?? 'pre_roll',
    durationSecs: json['duration_secs'] as int? ?? 15,
    status: json['status'] as String? ?? 'ready',
    ownerType: json['owner_type'] as String? ?? 'creator',
    categories: (json['categories'] as List<dynamic>?)?.map((e) => e.toString()).toList() ?? [],
    createdAt: DateTime.tryParse(json['created_at'] as String? ?? '') ?? DateTime.now(),
  );
}

/// Ad statistics for a creative.
class MMAdStats {
  final String adId;
  final int totalImpressions;
  final int completions;
  final int skips;
  final int clicks;
  final double completionRate;
  final double ctr;

  const MMAdStats({
    required this.adId,
    required this.totalImpressions,
    required this.completions,
    required this.skips,
    required this.clicks,
    required this.completionRate,
    required this.ctr,
  });

  factory MMAdStats.fromJson(Map<String, dynamic> json) => MMAdStats(
    adId: json['ad_id'] as String? ?? '',
    totalImpressions: json['total_impressions'] as int? ?? 0,
    completions: json['completions'] as int? ?? 0,
    skips: json['skips'] as int? ?? 0,
    clicks: json['clicks'] as int? ?? 0,
    completionRate: (json['completion_rate'] as num?)?.toDouble() ?? 0.0,
    ctr: (json['ctr'] as num?)?.toDouble() ?? 0.0,
  );
}

/// VoD ad policy attached to a recording response.
class MMAdPolicy {
  final MMAdDecision? preRoll;
  final List<MMAdDecision> midRolls;
  final MMAdDecision? postRoll;

  const MMAdPolicy({this.preRoll, this.midRolls = const [], this.postRoll});

  factory MMAdPolicy.fromJson(Map<String, dynamic> json) => MMAdPolicy(
    preRoll: json['pre_roll'] != null
        ? MMAdDecision.fromJson({'type': 'serve_ad', ...json['pre_roll'] as Map<String, dynamic>})
        : null,
    midRolls: (json['mid_rolls'] as List<dynamic>?)
        ?.map((e) => MMAdDecision.fromJson({'type': 'serve_ad', ...e as Map<String, dynamic>}))
        .toList() ?? [],
    postRoll: json['post_roll'] != null
        ? MMAdDecision.fromJson({'type': 'serve_ad', ...json['post_roll'] as Map<String, dynamic>})
        : null,
  );
}

/// Stream configuration for creating a stream.
class MMStreamConfig {
  final MMMediaType mediaType;
  final String? title;
  final bool e2ee;

  /// Minimum subscription tier required to view this stream (0 = open).
  /// If `null`, the host's `default_stream_min_tier` is used server-side.
  final int? minTier;

  const MMStreamConfig({
    this.mediaType = MMMediaType.audio,
    this.title,
    this.e2ee = false,
    this.minTier,
  });
}

// ---------------------------------------------------------------------------
// Lightning Network types (Phase 10 — LNBits)
// ---------------------------------------------------------------------------

/// Lightning invoice for a donation.
class MMLightningInvoice {
  /// BOLT11 invoice string (starts with "lnbc...")
  final String bolt11;
  /// Payment hash (unique identifier)
  final String paymentHash;
  /// Amount in satoshis
  final int amountSats;
  /// Approximate USD value
  final int approxUsdCents;

  const MMLightningInvoice({
    required this.bolt11,
    required this.paymentHash,
    required this.amountSats,
    this.approxUsdCents = 0,
  });

  /// Whether this looks like a valid bolt11 invoice
  bool get isValid => bolt11.startsWith('lnbc') || bolt11.startsWith('lntb');

  factory MMLightningInvoice.fromJson(Map<String, dynamic> json) => MMLightningInvoice(
    bolt11: json['checkout_url'] as String? ?? json['bolt11'] as String? ?? '',
    paymentHash: json['session_id'] as String? ?? json['payment_hash'] as String? ?? '',
    amountSats: json['amount_sats'] as int? ?? 0,
    approxUsdCents: json['approx_usd_cents'] as int? ?? 0,
  );
}

/// Available payment providers.
class MMPaymentProviders {
  final bool stripe;
  final bool lightning;

  const MMPaymentProviders({this.stripe = true, this.lightning = false});

  factory MMPaymentProviders.fromList(List<String> providers) => MMPaymentProviders(
    stripe: providers.contains('stripe'),
    lightning: providers.contains('lightning'),
  );
}
