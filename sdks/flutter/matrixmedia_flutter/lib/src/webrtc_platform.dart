/// Conditional export: uses `dart:html`-based implementation on web,
/// falls back to no-op stub on mobile (Android/iOS).
export 'webrtc_platform_stub.dart'
    if (dart.library.html) 'webrtc_platform_web.dart';
