import 'package:flutter/material.dart';
import 'package:livekit_client/livekit_client.dart';
import '../src/mm_stream.dart';

/// Renders video tracks from an [MMStream].
///
/// Shows all remote video tracks (camera + screen share) in a grid,
/// and optionally shows local preview for hosts.
class MMVideoRenderer extends StatelessWidget {
  final MMStream stream;
  final bool showLocalPreview;

  const MMVideoRenderer({
    super.key,
    required this.stream,
    this.showLocalPreview = true,
  });

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: stream,
      builder: (context, _) {
        final room = stream.room;
        if (room == null || !stream.connected) {
          return _placeholder('Not connected');
        }

        final videoWidgets = <Widget>[];

        // Remote video tracks
        for (final participant in room.remoteParticipants.values) {
          for (final pub in participant.videoTrackPublications) {
            if (pub.subscribed && pub.track != null) {
              videoWidgets.add(_videoTile(
                track: pub.track as VideoTrack,
                label: pub.source == TrackSource.screenShareVideo
                    ? 'Screen - ${participant.identity}'
                    : 'Camera - ${participant.identity}',
                isScreenShare: pub.source == TrackSource.screenShareVideo,
              ));
            }
          }
        }

        // Local video tracks (host preview)
        if (showLocalPreview && stream.isHost) {
          final local = room.localParticipant;
          if (local != null) {
            for (final pub in local.videoTrackPublications) {
              if (pub.track != null) {
                videoWidgets.add(_videoTile(
                  track: pub.track as VideoTrack,
                  label: pub.source == TrackSource.screenShareVideo
                      ? 'Your Screen'
                      : 'Your Camera',
                  isLocal: true,
                  isScreenShare: pub.source == TrackSource.screenShareVideo,
                ));
              }
            }
          }
        }

        if (videoWidgets.isEmpty) {
          return _placeholder(stream.connected ? 'Audio only' : 'Waiting for video...');
        }

        return LayoutBuilder(
          builder: (context, constraints) {
            final crossAxisCount = videoWidgets.length > 1 ? 2 : 1;
            return GridView.count(
              crossAxisCount: crossAxisCount,
              shrinkWrap: true,
              physics: const NeverScrollableScrollPhysics(),
              crossAxisSpacing: 4,
              mainAxisSpacing: 4,
              childAspectRatio: 16 / 9,
              children: videoWidgets,
            );
          },
        );
      },
    );
  }

  Widget _videoTile({
    required VideoTrack track,
    required String label,
    bool isLocal = false,
    bool isScreenShare = false,
  }) {
    return ClipRRect(
      borderRadius: BorderRadius.circular(8),
      child: Stack(
        fit: StackFit.expand,
        children: [
          VideoTrackRenderer(track),
          Positioned(
            left: 6,
            top: 6,
            child: Container(
              padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
              decoration: BoxDecoration(
                color: isLocal
                    ? Colors.indigo.withValues(alpha: 0.7)
                    : Colors.black.withValues(alpha: 0.6),
                borderRadius: BorderRadius.circular(4),
              ),
              child: Text(
                label,
                style: const TextStyle(color: Colors.white, fontSize: 11),
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _placeholder(String text) {
    return Container(
      height: 200,
      decoration: BoxDecoration(
        color: Colors.black,
        borderRadius: BorderRadius.circular(8),
      ),
      child: Center(
        child: Text(
          text,
          style: TextStyle(color: Colors.white.withValues(alpha: 0.5), fontSize: 14),
        ),
      ),
    );
  }
}
