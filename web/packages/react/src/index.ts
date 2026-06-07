// @matrixmedia/react — React provider, hooks, and components for MatrixMedia.

export { MMProvider } from "./MMProvider";
export type { MMProviderProps } from "./MMProvider";

export { MMClientContext } from "./context";

// Hooks
export { useMMClient } from "./hooks/useMMClient";
export { useRoomStreams } from "./hooks/useRoomStreams";
export type { UseRoomStreamsResult } from "./hooks/useRoomStreams";
export { useActiveStream } from "./hooks/useActiveStream";
export type {
  UseActiveStreamOptions,
  UseActiveStreamResult,
} from "./hooks/useActiveStream";
export { useViewer } from "./hooks/useViewer";
export type { UseViewerResult, ViewerState } from "./hooks/useViewer";
export { useHostPublisher } from "./hooks/useHostPublisher";
export type {
  UseHostPublisherResult,
  PublisherState,
} from "./hooks/useHostPublisher";

// Components
export { MMStream } from "./components/MMStream";
export type { MMStreamProps } from "./components/MMStream";
export { MMViewer } from "./components/MMViewer";
export type { MMViewerProps } from "./components/MMViewer";
export { MMHostControls } from "./components/MMHostControls";
export type { MMHostControlsProps } from "./components/MMHostControls";

// Re-export the common client types consumers need so they don't have to add
// a direct @matrixmedia/client import for typing props/returns.
export type {
  MMClient,
  MMClientOptions,
  MMError,
  StreamSummary,
  JoinStreamResponse,
  CreateStreamResponse,
  CreateStreamOptions,
  Tier,
  RecordingItem,
  DonationResult,
  DonateOptions,
  AdDecision,
  MediaType,
  StreamStatus,
  ErrorCode,
} from "@matrixmedia/client";
