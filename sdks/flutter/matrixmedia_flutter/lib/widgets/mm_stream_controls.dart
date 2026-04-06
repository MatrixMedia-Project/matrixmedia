import 'package:flutter/material.dart';
import '../src/mm_stream.dart';
// mm_types imported via mm_stream

/// Stream control overlay -- shows status, media buttons, and leave/end actions.
class MMStreamControls extends StatelessWidget {
  final MMStream stream;
  final VoidCallback? onLeave;
  final VoidCallback? onEnd;

  const MMStreamControls({
    super.key,
    required this.stream,
    this.onLeave,
    this.onEnd,
  });

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: stream,
      builder: (context, _) {
        return Container(
          padding: const EdgeInsets.all(12),
          decoration: BoxDecoration(
            color: Colors.black.withValues(alpha: 0.7),
            borderRadius: BorderRadius.circular(12),
          ),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              // Stream info bar
              _StreamInfoBar(stream: stream),
              const SizedBox(height: 12),

              // Media controls (host only)
              if (stream.isHost) ...[
                _HostMediaControls(stream: stream),
                const SizedBox(height: 12),
              ],

              // Leave / End button
              _ActionButtons(
                stream: stream,
                onLeave: onLeave,
                onEnd: onEnd,
              ),
            ],
          ),
        );
      },
    );
  }
}

class _StreamInfoBar extends StatelessWidget {
  final MMStream stream;
  const _StreamInfoBar({required this.stream});

  @override
  Widget build(BuildContext context) {
    return Row(
      children: [
        // LIVE badge
        if (stream.connected)
          Container(
            padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
            decoration: BoxDecoration(
              color: Colors.red,
              borderRadius: BorderRadius.circular(4),
            ),
            child: const Text(
              'LIVE',
              style: TextStyle(color: Colors.white, fontSize: 11, fontWeight: FontWeight.bold),
            ),
          ),
        if (!stream.connected)
          Container(
            padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
            decoration: BoxDecoration(
              color: Colors.grey,
              borderRadius: BorderRadius.circular(4),
            ),
            child: const Text(
              'OFFLINE',
              style: TextStyle(color: Colors.white, fontSize: 11),
            ),
          ),
        const SizedBox(width: 8),

        // Title
        Expanded(
          child: Text(
            stream.title,
            style: const TextStyle(color: Colors.white, fontWeight: FontWeight.w600),
            overflow: TextOverflow.ellipsis,
          ),
        ),

        // Viewer count
        Icon(Icons.people, color: Colors.white.withValues(alpha: 0.7), size: 16),
        const SizedBox(width: 4),
        Text(
          '${stream.viewerCount}',
          style: TextStyle(color: Colors.white.withValues(alpha: 0.7), fontSize: 13),
        ),
      ],
    );
  }
}

class _HostMediaControls extends StatelessWidget {
  final MMStream stream;
  const _HostMediaControls({required this.stream});

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisAlignment: MainAxisAlignment.center,
      children: [
        _MediaButton(
          icon: stream.micEnabled ? Icons.mic : Icons.mic_off,
          label: 'Mic',
          active: stream.micEnabled,
          onPressed: () => stream.setMicrophoneEnabled(!stream.micEnabled),
        ),
        const SizedBox(width: 12),
        _MediaButton(
          icon: stream.cameraEnabled ? Icons.videocam : Icons.videocam_off,
          label: 'Camera',
          active: stream.cameraEnabled,
          onPressed: () => stream.setCameraEnabled(!stream.cameraEnabled),
        ),
        const SizedBox(width: 12),
        _MediaButton(
          icon: stream.screenShareEnabled ? Icons.stop_screen_share : Icons.screen_share,
          label: 'Screen',
          active: stream.screenShareEnabled,
          onPressed: () => stream.setScreenShareEnabled(!stream.screenShareEnabled),
        ),
      ],
    );
  }
}

class _MediaButton extends StatelessWidget {
  final IconData icon;
  final String label;
  final bool active;
  final VoidCallback onPressed;

  const _MediaButton({
    required this.icon,
    required this.label,
    required this.active,
    required this.onPressed,
  });

  @override
  Widget build(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        IconButton(
          onPressed: onPressed,
          icon: Icon(icon),
          color: active ? Colors.white : Colors.white54,
          style: IconButton.styleFrom(
            backgroundColor: active ? Colors.indigo : Colors.white12,
            shape: const CircleBorder(),
            padding: const EdgeInsets.all(12),
          ),
        ),
        const SizedBox(height: 4),
        Text(
          label,
          style: TextStyle(
            color: active ? Colors.white : Colors.white54,
            fontSize: 11,
          ),
        ),
      ],
    );
  }
}

class _ActionButtons extends StatelessWidget {
  final MMStream stream;
  final VoidCallback? onLeave;
  final VoidCallback? onEnd;

  const _ActionButtons({required this.stream, this.onLeave, this.onEnd});

  @override
  Widget build(BuildContext context) {
    if (stream.isHost) {
      return SizedBox(
        width: double.infinity,
        child: ElevatedButton.icon(
          onPressed: onEnd ?? () => stream.end(),
          icon: const Icon(Icons.stop, size: 18),
          label: const Text('End Stream'),
          style: ElevatedButton.styleFrom(
            backgroundColor: Colors.red,
            foregroundColor: Colors.white,
            shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
          ),
        ),
      );
    }

    return SizedBox(
      width: double.infinity,
      child: OutlinedButton.icon(
        onPressed: onLeave ?? () => stream.leave(),
        icon: const Icon(Icons.exit_to_app, size: 18),
        label: const Text('Leave Stream'),
        style: OutlinedButton.styleFrom(
          foregroundColor: Colors.white70,
          side: const BorderSide(color: Colors.white30),
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
        ),
      ),
    );
  }
}
