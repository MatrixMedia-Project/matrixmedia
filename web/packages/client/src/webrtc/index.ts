// @matrixmedia/client/webrtc — LiveKit-backed stream viewer + publisher.
//
// `livekit-client` is an OPTIONAL peer dependency; import this subpath only
// when you need live WebRTC playback/publishing and have livekit-client
// installed.

export { StreamViewer } from "./StreamViewer";
export type {
  StreamViewerOptions,
  StreamViewerEvents,
  ViewerTrackEvent,
} from "./StreamViewer";

export { StreamPublisher } from "./StreamPublisher";
export type {
  StreamPublisherOptions,
  StreamPublisherEvents,
} from "./StreamPublisher";

export { Emitter } from "./emitter";
export type { Listener } from "./emitter";
