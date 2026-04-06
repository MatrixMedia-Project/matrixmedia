/// MatrixMedia error types.

class MMException implements Exception {
  final String code;
  final String message;
  final int? httpStatus;

  const MMException(this.code, this.message, [this.httpStatus]);

  @override
  String toString() => 'MMException($code): $message';

  // Common error constructors
  static MMException notAuthenticated() =>
      const MMException('MM_NOT_AUTHENTICATED', 'Not authenticated');
  static MMException streamNotFound() =>
      const MMException('MM_STREAM_NOT_FOUND', 'Stream not found', 404);
  static MMException streamFull() =>
      const MMException('MM_ROOM_FULL', 'Stream is at capacity', 409);
  static MMException forbidden(String detail) =>
      MMException('MM_FORBIDDEN', detail, 403);
  static MMException sfuUnavailable() =>
      const MMException('MM_SFU_UNAVAILABLE', 'SFU backend unreachable', 503);
  static MMException network(String detail) =>
      MMException('MM_NETWORK', detail);

  factory MMException.fromApiResponse(int status, Map<String, dynamic> body) {
    return MMException(
      body['error'] as String? ?? 'MM_UNKNOWN',
      body['message'] as String? ?? 'Unknown error',
      status,
    );
  }
}
