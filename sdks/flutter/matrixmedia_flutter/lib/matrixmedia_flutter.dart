/// MatrixMedia Flutter SDK
///
/// Provides live audio/video streaming, screen sharing, and recording
/// playback in Matrix rooms via the MatrixMedia backend (mm-core).
///
/// Usage:
/// ```dart
/// final client = MMClient(
///   serverUrl: 'http://10.0.0.105:6167',
///   tokenProvider: () async => await getOpenIdToken(),
/// );
/// await client.authenticate(openIdToken);
/// final stream = await client.joinStream(roomId);
/// ```
library matrixmedia_flutter;

export 'src/mm_client.dart';
export 'src/mm_stream.dart';
export 'src/mm_types.dart';
export 'src/mm_error.dart';
export 'src/mm_api_client.dart';
export 'widgets/mm_audio_renderer.dart';
export 'widgets/mm_video_renderer.dart';
export 'widgets/mm_stream_controls.dart';
