import 'package:flutter/material.dart';
import '../src/mm_stream.dart';

/// Audio-only visualization for an [MMStream].
///
/// Shows a simple audio level indicator or waveform animation
/// when audio tracks are active.
class MMAudioRenderer extends StatelessWidget {
  final MMStream stream;
  final Color barColor;
  final double height;

  const MMAudioRenderer({
    super.key,
    required this.stream,
    this.barColor = Colors.indigo,
    this.height = 80,
  });

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: stream,
      builder: (context, _) {
        final isActive = stream.connected;

        return Container(
          height: height,
          decoration: BoxDecoration(
            color: Colors.black87,
            borderRadius: BorderRadius.circular(8),
          ),
          child: Center(
            child: isActive
                ? _AudioBars(color: barColor)
                : Text(
                    'No audio',
                    style: TextStyle(color: Colors.white.withValues(alpha: 0.4)),
                  ),
          ),
        );
      },
    );
  }
}

/// Simple animated audio bars (placeholder for real FFT data).
class _AudioBars extends StatefulWidget {
  final Color color;
  const _AudioBars({required this.color});

  @override
  State<_AudioBars> createState() => _AudioBarsState();
}

class _AudioBarsState extends State<_AudioBars> with TickerProviderStateMixin {
  late final AnimationController _controller;

  @override
  void initState() {
    super.initState();
    _controller = AnimationController(
      duration: const Duration(milliseconds: 800),
      vsync: this,
    )..repeat(reverse: true);
  }

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return AnimatedBuilder(
      animation: _controller,
      builder: (context, child) {
        return Row(
          mainAxisAlignment: MainAxisAlignment.center,
          crossAxisAlignment: CrossAxisAlignment.end,
          children: List.generate(12, (i) {
            final offset = (i * 0.15 + _controller.value) % 1.0;
            final h = 10 + (offset * 40);
            return Container(
              width: 4,
              height: h,
              margin: const EdgeInsets.symmetric(horizontal: 2),
              decoration: BoxDecoration(
                color: widget.color.withValues(alpha: 0.5 + offset * 0.5),
                borderRadius: BorderRadius.circular(2),
              ),
            );
          }),
        );
      },
    );
  }
}
