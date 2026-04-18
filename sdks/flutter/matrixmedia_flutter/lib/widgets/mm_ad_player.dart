import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:crypto/crypto.dart';
import 'dart:convert';

import '../src/mm_api_client.dart';
import '../src/mm_types.dart';

// Conditional import: web gets real HTML video element helpers,
// mobile gets no-op stubs.
import 'mm_ad_player_stub.dart'
    if (dart.library.html) 'mm_ad_player_web.dart';

/// Reusable ad player widget for MatrixMedia.
///
/// Plays a video ad, tracks quartile progress, shows skip button after
/// configured delay, and submits HMAC completion proof to the server.
///
/// Works on both web (HTML5 <video>) and mobile (timer-based placeholder).
class MMAdPlayer extends StatefulWidget {
  final MMAdDecision decision;
  final MMApiClient api;
  final String streamId;
  final VoidCallback onComplete;
  final VoidCallback? onSkip;
  final VoidCallback? onError;

  const MMAdPlayer({
    super.key,
    required this.decision,
    required this.api,
    required this.streamId,
    required this.onComplete,
    this.onSkip,
    this.onError,
  });

  @override
  State<MMAdPlayer> createState() => _MMAdPlayerState();
}

class _MMAdPlayerState extends State<MMAdPlayer> {
  late final String _viewType;
  PlatformAdVideo? _platformVideo;
  Timer? _progressTimer;
  double _progress = 0.0; // 0.0 to 1.0
  int _elapsed = 0;
  bool _canSkip = false;
  bool _completed = false;
  final Set<String> _reportedEvents = {};

  int get _duration => widget.decision.ad?.durationSecs ?? 15;
  int get _skipAfter => widget.decision.skipAfterSecs ?? 5;
  String get _impressionToken => widget.decision.impressionToken ?? '';
  String get _challenge => widget.decision.challenge ?? '';
  String get _viewerSecret => widget.decision.viewerSecret ?? '';
  String get _adId => widget.decision.ad?.adId ?? '';

  @override
  void initState() {
    super.initState();
    if (widget.decision.hasAd) {
      _viewType = 'mm-ad-${_impressionToken.hashCode}';

      // Create the platform-specific video element (real on web, stub on mobile).
      _platformVideo = createAdVideoElement(
        mediaUrl: widget.decision.ad!.mediaUrl,
        viewType: _viewType,
        onEnded: _onVideoEnded,
        onError: _onVideoError,
      );

      // Report impression.
      _reportEvent('impression');

      // Start progress tracking.
      _progressTimer = Timer.periodic(const Duration(milliseconds: 500), (_) {
        _updateProgress();
      });
    }
  }

  void _updateProgress() {
    if (_completed) return;

    final videoEl = _platformVideo?.videoElement;
    if (kIsWeb && videoEl != null) {
      // Web: read currentTime/duration from the HTML video element.
      final currentTime = (videoEl.currentTime as num).toDouble();
      final videoDuration = (videoEl.duration as num).toDouble();
      if (videoDuration.isNaN || videoDuration <= 0) return;

      setState(() {
        _progress = currentTime / videoDuration;
        _elapsed = currentTime.toInt();
        _canSkip = _elapsed >= _skipAfter;
      });
    } else {
      // Mobile: simulate progress based on wall-clock time.
      setState(() {
        _elapsed++;
        _progress = (_elapsed / _duration).clamp(0.0, 1.0);
        _canSkip = _elapsed >= _skipAfter;
      });
      if (_elapsed >= _duration && !_completed) {
        _onVideoEnded();
        return;
      }
    }

    // Report quartile events.
    if (_progress >= 0.25 && !_reportedEvents.contains('quartile_25')) {
      _reportEvent('quartile_25');
    }
    if (_progress >= 0.50 && !_reportedEvents.contains('quartile_50')) {
      _reportEvent('quartile_50');
    }
    if (_progress >= 0.75 && !_reportedEvents.contains('quartile_75')) {
      _reportEvent('quartile_75');
    }
  }

  void _reportEvent(String event) {
    if (_reportedEvents.contains(event)) return;
    _reportedEvents.add(event);

    widget.api.reportAdEvent(
      impressionToken: _impressionToken,
      event: event,
      positionSecs: _elapsed,
    ).catchError((_) {}); // best-effort
  }

  void _onVideoEnded() {
    if (_completed) return;
    _completed = true;
    _reportEvent('completed');
    _submitCompletionProof();
  }

  void _onVideoError() {
    _reportEvent('error');
    widget.onError?.call();
    // Auto-skip on error after 2 seconds.
    Future.delayed(const Duration(seconds: 2), () {
      if (mounted && !_completed) {
        _completed = true;
        widget.onComplete();
      }
    });
  }

  void _onSkip() {
    if (_completed) return;
    _completed = true;
    _reportEvent('skipped');
    _submitCompletionProof();
    widget.onSkip?.call();
  }

  void _submitCompletionProof() {
    final timestamp = DateTime.now().millisecondsSinceEpoch ~/ 1000;
    final response = _computeHmac(_challenge, _adId, timestamp, _viewerSecret);

    widget.api.submitAdComplete(
      widget.streamId,
      impressionToken: _impressionToken,
      challengeResponse: response,
      timestamp: timestamp,
    ).then((_) {
      if (mounted) widget.onComplete();
    }).catchError((_) {
      if (mounted) widget.onComplete(); // complete anyway on proof failure
    });
  }

  String _computeHmac(String nonce, String adId, int timestamp, String secret) {
    final secretBytes = _hexDecode(secret);
    final message = utf8.encode('$nonce$adId$timestamp');
    final hmacSha256 = Hmac(sha256, secretBytes);
    final digest = hmacSha256.convert(message);
    return digest.toString();
  }

  List<int> _hexDecode(String hex) {
    final result = <int>[];
    for (var i = 0; i < hex.length; i += 2) {
      result.add(int.parse(hex.substring(i, i + 2), radix: 16));
    }
    return result;
  }

  @override
  void dispose() {
    _progressTimer?.cancel();
    _platformVideo?.pause();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    if (!widget.decision.hasAd) {
      // No ad to show -- complete immediately.
      WidgetsBinding.instance.addPostFrameCallback((_) => widget.onComplete());
      return const SizedBox.shrink();
    }

    return Container(
      color: Colors.black,
      child: Stack(
        children: [
          // Video player
          if (kIsWeb)
            Positioned.fill(
              child: HtmlElementView(viewType: _viewType),
            )
          else
            Center(
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  const Icon(Icons.play_circle_outline, color: Colors.white54, size: 48),
                  const SizedBox(height: 8),
                  Text(
                    widget.decision.ad?.title ?? 'Advertisement',
                    style: const TextStyle(color: Colors.white, fontSize: 14),
                  ),
                ],
              ),
            ),

          // Top bar: "Ad" badge + countdown
          Positioned(
            top: 8,
            left: 8,
            right: 8,
            child: Row(
              children: [
                Container(
                  padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
                  decoration: BoxDecoration(
                    color: Colors.amber.shade700,
                    borderRadius: BorderRadius.circular(4),
                  ),
                  child: const Text('AD', style: TextStyle(color: Colors.black, fontWeight: FontWeight.bold, fontSize: 12)),
                ),
                const SizedBox(width: 8),
                Text(
                  '${(_duration - _elapsed).clamp(0, _duration)}s',
                  style: const TextStyle(color: Colors.white70, fontSize: 12),
                ),
                const Spacer(),
                if (widget.decision.ad?.clickThroughUrl != null)
                  TextButton(
                    onPressed: () {
                      _reportEvent('clicked');
                      openClickThrough(widget.decision.ad!.clickThroughUrl!);
                    },
                    child: const Text('Learn More', style: TextStyle(color: Colors.amber)),
                  ),
              ],
            ),
          ),

          // Progress bar
          Positioned(
            bottom: 0,
            left: 0,
            right: 0,
            child: LinearProgressIndicator(
              value: _progress,
              backgroundColor: Colors.white24,
              valueColor: const AlwaysStoppedAnimation<Color>(Colors.amber),
              minHeight: 3,
            ),
          ),

          // Skip button
          if (_canSkip || _skipAfter == 0)
            Positioned(
              bottom: 16,
              right: 16,
              child: ElevatedButton.icon(
                onPressed: _onSkip,
                icon: const Icon(Icons.skip_next, size: 18),
                label: const Text('Skip Ad'),
                style: ElevatedButton.styleFrom(
                  backgroundColor: Colors.white.withValues(alpha: 0.2),
                  foregroundColor: Colors.white,
                ),
              ),
            )
          else
            Positioned(
              bottom: 16,
              right: 16,
              child: Container(
                padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
                decoration: BoxDecoration(
                  color: Colors.black54,
                  borderRadius: BorderRadius.circular(4),
                ),
                child: Text(
                  'Skip in ${(_skipAfter - _elapsed).clamp(0, _skipAfter)}s',
                  style: const TextStyle(color: Colors.white70, fontSize: 12),
                ),
              ),
            ),
        ],
      ),
    );
  }
}
