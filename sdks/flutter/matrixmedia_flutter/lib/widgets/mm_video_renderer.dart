import 'package:flutter/material.dart';
import 'package:livekit_client/livekit_client.dart';
import '../src/mm_stream.dart';

// Conditional import: web uses HTML video element,
// mobile uses flutter_webrtc RTCVideoView.
import 'switch_video_stub.dart'
    if (dart.library.html) 'switch_video_web.dart';

/// Renders video from an [MMStream].
/// Supports LiveKit mode (Room) and mm-switch mode (direct WebRTC MediaStream).
/// On mobile, mm-switch mode is not available -- falls back to LiveKit.
class MMVideoRenderer extends StatelessWidget {
  final MMStream stream;
  final bool showLocalPreview;

  const MMVideoRenderer({super.key, required this.stream, this.showLocalPreview = true});

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: stream,
      builder: (context, _) {
        // mm-switch host mode: render the local publisher's camera preview.
        // alwaysMuted=true prevents the host from hearing their own mic
        // (the <video> element would otherwise play back the captured audio).
        // mm-switch host mode: render the local publisher's camera preview.
        // alwaysMuted=true prevents the host from hearing their own mic.
        if (stream.isHost && stream.publisherStream != null) {
          return buildSwitchVideo(stream.publisherStream, alwaysMuted: true);
        }

        // mm-switch viewer mode: render the viewer's incoming MediaStream.
        if (stream.usesSwitch) {
          final ms = stream.switchMediaStream;
          if (ms != null) {
            return buildSwitchVideo(ms);
          }
          return _placeholder(stream.connected ? 'Connecting video...' : 'Not connected');
        }

        // LiveKit mode
        final room = stream.room;
        if (room == null || !stream.connected) return _placeholder('Not connected');

        final videoWidgets = <Widget>[];
        for (final p in room.remoteParticipants.values) {
          for (final pub in p.videoTrackPublications) {
            if (pub.subscribed && pub.track != null) {
              videoWidgets.add(_videoTile(
                track: pub.track as VideoTrack,
                label: pub.source == TrackSource.screenShareVideo
                    ? 'Screen - ${p.identity}' : 'Camera - ${p.identity}',
                isScreenShare: pub.source == TrackSource.screenShareVideo,
              ));
            }
          }
        }
        if (showLocalPreview && stream.isHost && room.localParticipant != null) {
          for (final pub in room.localParticipant!.videoTrackPublications) {
            if (pub.track != null) {
              videoWidgets.add(_videoTile(
                track: pub.track as VideoTrack,
                label: pub.source == TrackSource.screenShareVideo ? 'Your Screen' : 'Your Camera',
                isLocal: true, isScreenShare: pub.source == TrackSource.screenShareVideo,
              ));
            }
          }
        }
        if (videoWidgets.isEmpty) return _placeholder(stream.connected ? 'Audio only' : 'Waiting...');
        return LayoutBuilder(builder: (ctx, c) {
          final cols = videoWidgets.length > 1 ? 2 : 1;
          return GridView.count(
            crossAxisCount: cols, shrinkWrap: true,
            physics: const NeverScrollableScrollPhysics(),
            crossAxisSpacing: 4, mainAxisSpacing: 4, childAspectRatio: 16 / 9,
            children: videoWidgets,
          );
        });
      },
    );
  }

  Widget _videoTile({required VideoTrack track, required String label, bool isLocal = false, bool isScreenShare = false}) {
    return ClipRRect(
      borderRadius: BorderRadius.circular(8),
      child: Stack(fit: StackFit.expand, children: [
        VideoTrackRenderer(track),
        Positioned(left: 6, top: 6, child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
          decoration: BoxDecoration(
            color: isLocal ? Colors.indigo.withValues(alpha: 0.7) : Colors.black.withValues(alpha: 0.6),
            borderRadius: BorderRadius.circular(4),
          ),
          child: Text(label, style: const TextStyle(color: Colors.white, fontSize: 11)),
        )),
      ]),
    );
  }

  Widget _placeholder(String text) {
    return Container(
      height: 200,
      decoration: BoxDecoration(color: Colors.black, borderRadius: BorderRadius.circular(8)),
      child: Center(child: Text(text, style: TextStyle(color: Colors.white.withValues(alpha: 0.5), fontSize: 14))),
    );
  }
}
