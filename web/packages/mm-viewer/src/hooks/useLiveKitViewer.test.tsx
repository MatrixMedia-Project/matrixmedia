import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useLiveKitViewer } from './useLiveKitViewer';

vi.mock('@matrixmedia/client/webrtc', () => {
  const handlers = new Map<string, Array<(arg?: unknown) => void>>();
  class MockStreamViewer {
    on(event: string, cb: (arg?: unknown) => void) {
      const list = handlers.get(event) ?? [];
      list.push(cb);
      handlers.set(event, list);
      return this;
    }
    async connect() {}
    async disconnect() {}
    get room() {
      return null;
    }
  }
  return {
    StreamViewer: MockStreamViewer,
    __emitHandlers: handlers,
  };
});

const webrtcMock = (await import(
  '@matrixmedia/client/webrtc'
)) as unknown as {
  __emitHandlers: Map<string, Array<(arg?: unknown) => void>>;
};

function emit(event: string, arg?: unknown) {
  for (const cb of webrtcMock.__emitHandlers.get(event) ?? []) cb(arg);
}

function fakeStream(withAudio: boolean): MediaStream {
  return {
    getAudioTracks: () => (withAudio ? [{} as MediaStreamTrack] : []),
  } as unknown as MediaStream;
}

const join = {
  sfuUrl: 'wss://sfu',
  sfuToken: 'jwt',
  participantId: 'p1',
  switchUrl: 'https://hs/_mm/switch',
  switchSourceId: 'stream-s1',
  switchViewerId: 'viewer-s1',
} as never;

beforeEach(() => {
  webrtcMock.__emitHandlers.clear();
  document.body.innerHTML = '';
  HTMLMediaElement.prototype.play = vi.fn().mockResolvedValue(undefined);
  const analyser = { fftSize: 0, smoothingTimeConstant: 0 };
  const audioContext = {
    state: 'running',
    resume: vi.fn().mockResolvedValue(undefined),
    close: vi.fn().mockResolvedValue(undefined),
    createMediaStreamSource: vi.fn(() => ({ connect: vi.fn() })),
    createAnalyser: vi.fn(() => analyser),
  };
  vi.stubGlobal(
    'AudioContext',
    vi.fn(() => audioContext),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

/**
 * Regression test: on the mm-switch transport the server forwards video
 * and audio as separate MediaStreams, so the <video> element is
 * video-only. The hook must attach the audio stream to its own hidden
 * <audio> element (as the LiveKit path does) or video sessions play
 * silently.
 */
describe('useLiveKitViewer mm-switch audio', () => {
  it('plays the switch audio stream through a hidden audio element', async () => {
    const { result } = renderHook(() => useLiveKitViewer());
    await act(async () => {
      await result.current.connect(join);
    });

    const audioStream = fakeStream(true);
    const videoStream = fakeStream(false);

    act(() => {
      emit('switchTrack', { kind: 'audio', mediaStream: audioStream });
    });
    act(() => {
      emit('switchTrack', { kind: 'video', mediaStream: videoStream });
    });

    const audioEl = document.body.querySelector(
      'audio',
    ) as HTMLAudioElement | null;
    expect(audioEl).not.toBeNull();
    expect(audioEl!.srcObject).toBe(audioStream);
    expect(audioEl!.style.display).toBe('none');

    expect(result.current.videoElement).not.toBeNull();
    expect(result.current.videoElement!.srcObject).toBe(videoStream);
    expect(result.current.analyserNode).not.toBeNull();
  });

  it('does not double-play a stream the video element already carries', async () => {
    const { result } = renderHook(() => useLiveKitViewer());
    await act(async () => {
      await result.current.connect(join);
    });

    const shared = fakeStream(true);
    act(() => {
      emit('switchTrack', { kind: 'video', mediaStream: shared });
    });
    act(() => {
      emit('switchTrack', { kind: 'audio', mediaStream: shared });
    });

    expect(document.body.querySelector('audio')).toBeNull();
    expect(result.current.videoElement!.srcObject).toBe(shared);
  });
});
