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
  });

  bool get isActive => status == MMStreamStatus.active;

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
      );
}

/// Join stream response.
class MMJoinResult {
  final String sfuUrl;
  final String sfuToken;
  final String participantId;
  final MME2eeInfo? e2ee;

  const MMJoinResult({
    required this.sfuUrl,
    required this.sfuToken,
    required this.participantId,
    this.e2ee,
  });

  factory MMJoinResult.fromJson(Map<String, dynamic> json) => MMJoinResult(
        sfuUrl: json['sfu_url'] as String? ?? '',
        sfuToken: json['sfu_token'] as String? ?? '',
        participantId: json['participant_id'] as String? ?? '',
        e2ee: json['e2ee'] != null ? MME2eeInfo.fromJson(json['e2ee'] as Map<String, dynamic>) : null,
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
      );
}

/// Stream configuration for creating a stream.
class MMStreamConfig {
  final MMMediaType mediaType;
  final String? title;
  final bool e2ee;

  const MMStreamConfig({
    this.mediaType = MMMediaType.audio,
    this.title,
    this.e2ee = false,
  });
}
