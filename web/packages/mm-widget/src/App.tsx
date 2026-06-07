import { createSignal, createEffect, onCleanup, Show } from 'solid-js';
import type { WidgetState, PaywallInfo } from './types';
import { MMApiClient, MMApiError } from './api/MMApiClient';
import { WidgetAuth } from './auth/WidgetAuth';
import { useWidgetApi, getParentOrigin, getApiBaseUrl } from './hooks/useWidgetApi';
import { useStreamState } from './hooks/useStreamState';
import { useLiveKitRoom } from './hooks/useLiveKitRoom';
import { useRecordings } from './hooks/useRecordings';
import { useDonations } from './hooks/useDonations';
import { useEntitlement } from './hooks/useEntitlement';
import { WidgetShell } from './components/WidgetShell';
import { StreamStatus } from './components/StreamStatus';
import { AudioVisualizer } from './components/AudioVisualizer';
import { VideoPlayer } from './components/VideoPlayer';
import { JoinLeaveButton } from './components/JoinLeaveButton';
import { HostControls } from './components/HostControls';
import { VolumeControl } from './components/VolumeControl';
import { RecordingList } from './components/RecordingList';
import { DonationOverlay } from './components/DonationOverlay';
import { DonateButton } from './components/DonateButton';
import { PaywallOverlay } from './components/PaywallOverlay';

/**
 * Root component with state machine:
 *
 * loading -> (auth success) -> authenticated -> (check stream) -> idle | streaming
 * idle -> (user clicks "Go Live") -> hosting
 * idle -> (stream detected) -> joining -> streaming
 * hosting -> (user clicks "End") -> idle
 * streaming -> (user clicks "Leave") -> idle
 * any -> (error) -> error -> (retry) -> loading
 */
/**
 * Props for embedded (Custom Element) usage. When omitted, the widget runs
 * in its original iframe/SPA mode (config from URL params + postMessage
 * OpenID auth), which is the path mm-core serves in production.
 */
export interface AppProps {
  /** Matrix room id. Bypasses URL-param parsing when set. */
  room?: string;
  /** mm-core server base URL. Overrides the origin-derived base when set. */
  server?: string;
  /**
   * Pre-obtained MM session token. When set, the widget skips the parent
   * Element postMessage OpenID handshake and uses this token directly.
   */
  token?: string;
}

export function App(props: AppProps = {}) {
  const { params, ready } = useWidgetApi(props.room ? { roomId: props.room } : undefined);

  const [state, setState] = createSignal<WidgetState>('loading');
  const [errorMsg, setErrorMsg] = createSignal<string | null>(null);
  const [currentStreamId, setCurrentStreamId] = createSignal<string | null>(null);
  const [isHost, setIsHost] = createSignal(false);

  // Embedded mode is driven by a pre-supplied token; iframe/SPA mode resolves
  // its token through the WidgetAuth postMessage OpenID flow.
  const embeddedToken = props.token ? props.token : null;

  // API client -- token getter is wired to auth (or the embedded token).
  let auth: WidgetAuth | null = null;
  const apiBaseUrl = getApiBaseUrl(props.server);
  const api = new MMApiClient(apiBaseUrl, () => embeddedToken ?? auth?.getToken() ?? null);

  // Stream polling
  const streamState = useStreamState(api, () => params()?.roomId ?? null);

  // LiveKit room connection
  const livekit = useLiveKitRoom();

  // Recordings (VoD)
  const recordings = useRecordings(api, () => params()?.roomId ?? null);

  // Donations
  const donationFeed = useDonations(api, () => currentStreamId());

  // Entitlement (subscription gating)
  const entitlement = useEntitlement(api);
  const [paywallInfo, setPaywallInfo] = createSignal<PaywallInfo | null>(null);

  // Host media toggles
  const [cameraEnabled, setCameraEnabled] = createSignal(false);
  const [screenEnabled, setScreenEnabled] = createSignal(false);

  async function handleToggleCamera() {
    if (cameraEnabled()) {
      await livekit.disableCamera();
      setCameraEnabled(false);
    } else {
      await livekit.enableCamera();
      setCameraEnabled(true);
    }
  }

  async function handleToggleScreen() {
    if (screenEnabled()) {
      await livekit.disableScreenShare();
      setScreenEnabled(false);
    } else {
      await livekit.enableScreenShare();
      setScreenEnabled(true);
    }
  }

  // ---------------------------------------------------------------------------
  // Auth flow
  // ---------------------------------------------------------------------------

  async function doAuth() {
    setState('loading');
    setErrorMsg(null);

    const p = params();
    if (!p) {
      setErrorMsg('Missing widget parameters (roomId)');
      setState('error');
      return;
    }

    // Embedded (Custom Element) mode: a session token was supplied directly,
    // so skip the parent Element postMessage OpenID handshake entirely.
    if (embeddedToken) {
      setState('authenticated');
      streamState.start();
      recordings.refresh();
      return;
    }

    const parentOrigin = getParentOrigin(p.parentUrl);

    // Clean up previous auth
    if (auth) auth.destroy();

    auth = new WidgetAuth(api, parentOrigin, p.widgetId);

    try {
      await auth.authenticate();
      setState('authenticated');

      // Start polling for streams
      streamState.start();

      // Fetch recent recordings
      recordings.refresh();
    } catch (err) {
      const msg = err instanceof Error ? err.message : 'Authentication failed';
      setErrorMsg(msg);
      setState('error');
    }
  }

  // Auto-auth when widget API is ready
  createEffect(() => {
    if (ready() && params()) {
      doAuth();
    }
  });

  // ---------------------------------------------------------------------------
  // Transition from authenticated -> idle/streaming based on poll results
  // ---------------------------------------------------------------------------

  createEffect(() => {
    const s = streamState.stream();
    const currentState = state();

    if (currentState === 'authenticated' || currentState === 'idle') {
      if (s && s.status === 'active') {
        // A stream is active -- user can join
        // If we're the host, go to hosting
        if (isHost() && currentStreamId() === s.stream_id) {
          setState('hosting');
        }
        // Otherwise stay idle (user needs to click Join)
        else if (currentState === 'authenticated') {
          setState('idle');
        }
      } else if (currentState === 'authenticated') {
        setState('idle');
      }
    }
  });

  // ---------------------------------------------------------------------------
  // Start/stop donation feed when stream connection changes
  // ---------------------------------------------------------------------------

  createEffect(() => {
    const s = state();
    if ((s === 'streaming' || s === 'hosting') && currentStreamId()) {
      donationFeed.start();
    } else {
      donationFeed.stop();
    }
  });

  // ---------------------------------------------------------------------------
  // Actions
  // ---------------------------------------------------------------------------

  async function handleGoLive(title: string, e2ee: boolean) {
    const p = params();
    if (!p) return;

    try {
      setState('hosting');
      const stream = await api.createStream(
        p.roomId,
        title || undefined,
        'audio',
        e2ee,
      );
      setCurrentStreamId(stream.stream_id);
      setIsHost(true);

      // Connect to LiveKit as the host (enables microphone).
      // If mm-core returned E2EE key material, configure LiveKit's
      // Insertable Streams pipeline with the shared room key.
      await livekit.connectAsHost({
        sfuUrl: stream.sfu_url,
        sfuToken: stream.sfu_token,
        e2ee: stream.e2ee?.enabled
          ? { keyB64: stream.e2ee.key_b64, keyId: stream.e2ee.key_id }
          : undefined,
      });

      streamState.stop(); // Stop polling while hosting
      streamState.refresh();
    } catch (err) {
      const msg = err instanceof Error ? err.message : 'Failed to start stream';
      setErrorMsg(msg);
      setState('error');
      await livekit.disconnect();
    }
  }

  async function handleJoin() {
    const s = streamState.stream();
    if (!s) return;

    try {
      setState('joining');
      const joinResp = await api.joinStream(s.stream_id);
      setCurrentStreamId(s.stream_id);
      setIsHost(false);

      // Connect to LiveKit as a listener, programming E2EE if the
      // stream is end-to-end encrypted.
      await livekit.connect({
        sfuUrl: joinResp.sfu_url,
        sfuToken: joinResp.sfu_token,
        e2ee: joinResp.e2ee?.enabled
          ? { keyB64: joinResp.e2ee.key_b64, keyId: joinResp.e2ee.key_id }
          : undefined,
      });

      setState('streaming');
      setPaywallInfo(null);
      // Stop polling while connected
      streamState.stop();
    } catch (err) {
      // 402 MM_CONTENT_GATED -- show paywall overlay instead of error
      if (err instanceof MMApiError && err.statusCode === 402 && err.code === 'MM_CONTENT_GATED') {
        const d = err.data;
        setPaywallInfo({
          tier_name: (d.tier_name as string) ?? 'Premium',
          tier_level: (d.tier_level as number) ?? 1,
          price_cents: (d.price_cents as number) ?? 999,
          currency: (d.currency as string) ?? 'USD',
          preview_seconds: (d.preview_seconds as number) ?? 0,
          checkout_url: (d.checkout_url as string) ?? '',
          creator_user_id: s.host_user_id,
        });
        setState('idle');
        return;
      }

      const msg = err instanceof Error ? err.message : 'Failed to join stream';
      setErrorMsg(msg);
      setState('error');
      await livekit.disconnect();
    }
  }

  async function handleLeave() {
    const sid = currentStreamId();
    if (!sid) return;

    // Disconnect from LiveKit first
    await livekit.disconnect();

    try {
      await api.leaveStream(sid);
    } catch {
      // Best-effort leave
    }

    setCurrentStreamId(null);
    setIsHost(false);
    setState('idle');
    // Resume polling
    streamState.start();
  }

  async function handleEndStream() {
    const sid = currentStreamId();
    if (!sid) return;

    // Disconnect from LiveKit first
    await livekit.disconnect();

    try {
      await api.endStream(sid);
    } catch {
      // Best-effort end
    }

    setCurrentStreamId(null);
    setIsHost(false);
    setState('idle');
    streamState.start();
    streamState.refresh();
    // Refresh recordings after ending -- the just-finished stream may appear.
    recordings.refresh();
  }

  function handlePaywallDismiss() {
    setPaywallInfo(null);
  }

  function handlePaywallSubscribe() {
    // After the user subscribes in the new tab, invalidate the
    // entitlement cache so the next join attempt re-checks.
    const pw = paywallInfo();
    if (pw) {
      entitlement.invalidate(pw.creator_user_id);
    }
  }

  function handleRetry() {
    doAuth();
  }

  // Cleanup
  onCleanup(() => {
    if (auth) auth.destroy();
    streamState.stop();
    livekit.disconnect();
  });

  // ---------------------------------------------------------------------------
  // Render
  // ---------------------------------------------------------------------------

  const isAuthenticated = () => state() !== 'loading' && state() !== 'error';
  const isLoading = () => state() === 'loading';
  const hasActiveStream = () => {
    const s = streamState.stream();
    return s !== null && s.status === 'active';
  };
  const isActive = () =>
    state() === 'streaming' || state() === 'hosting' || state() === 'joining';

  return (
    <WidgetShell
      loading={isLoading()}
      error={errorMsg()}
      authenticated={isAuthenticated()}
      onRetry={handleRetry}
    >
      <div class="mm-widget">
        {/* Stream status bar */}
        <StreamStatus stream={streamState.stream()} />

        {/* E2EE indicator (shown when encryption is active) */}
        <Show when={livekit.e2eeEnabled() && isActive()}>
          <div class="mm-e2ee-indicator" title="End-to-end encrypted">
            <span class="mm-e2ee-indicator__icon" aria-hidden="true">🔒</span>
            <span class="mm-e2ee-indicator__text">End-to-end encrypted</span>
          </div>
        </Show>

        {/* Video player or audio visualizer -- wrapped for donation overlay positioning */}
        <div style={{ position: 'relative' }}>
          {(livekit.remoteVideoTrack() || livekit.videoTrack()) ? (
            <VideoPlayer
              videoElement={livekit.remoteVideoTrack() || livekit.videoTrack()}
              isScreenShare={livekit.isScreenShare()}
            />
          ) : (
            <AudioVisualizer analyserNode={livekit.analyserNode()} active={isActive()} />
          )}

          {/* Donation overlay (visible when stream is active) */}
          <Show when={isActive()}>
            <DonationOverlay donations={donationFeed.donations()} />
          </Show>
        </div>

        {/* LiveKit reconnecting indicator */}
        <Show when={livekit.reconnecting()}>
          <div class="mm-reconnecting">Reconnecting...</div>
        </Show>

        {/* Controls */}
        <div class="mm-controls">
          {/* Volume (shown when streaming as a listener) */}
          <Show when={state() === 'streaming'}>
            <VolumeControl
              onVolumeChange={(vol) => {
                livekit.setVolume(vol);
                livekit.setMuted(vol === 0);
              }}
            />
          </Show>

          {/* Donate button (viewer, shown during active stream) */}
          <Show when={state() === 'streaming' && currentStreamId()}>
            <DonateButton api={api} streamId={currentStreamId()!} />
          </Show>

          {/* Join/Leave button (viewer) */}
          <JoinLeaveButton
            state={state()}
            hasStream={hasActiveStream()}
            onJoin={handleJoin}
            onLeave={handleLeave}
          />
        </div>

        {/* Host controls */}
        <HostControls
          state={state()}
          isHost={isHost()}
          stream={streamState.stream()}
          cameraEnabled={cameraEnabled()}
          screenEnabled={screenEnabled()}
          onGoLive={handleGoLive}
          onEndStream={handleEndStream}
          onToggleCamera={handleToggleCamera}
          onToggleScreen={handleToggleScreen}
        />

        {/* Polling error (non-fatal) */}
        <Show when={streamState.error() && state() !== 'error'}>
          <div class="mm-error" style={{ 'font-size': '12px', 'text-align': 'center' }}>
            {streamState.error()}
          </div>
        </Show>

        {/* LiveKit connection error (non-fatal) */}
        <Show when={livekit.error() && state() !== 'error'}>
          <div class="mm-error" style={{ 'font-size': '12px', 'text-align': 'center' }}>
            SFU: {livekit.error()}
          </div>
        </Show>

        {/* Recent recordings (VoD)
            - prominent when idle (no active stream)
            - collapsed while actively streaming/hosting/joining */}
        <Show when={isAuthenticated() && state() !== 'error'}>
          <RecordingList
            recordings={recordings.recordings()}
            loading={recordings.loading()}
            error={recordings.error()}
            hasMore={recordings.hasMore()}
            onLoadMore={recordings.loadMore}
            collapsed={isActive() || state() === 'hosting'}
          />
        </Show>
        {/* Paywall overlay (shown when join is gated) */}
        <Show when={paywallInfo()}>
          {(pw) => (
            <PaywallOverlay
              paywall={pw()}
              onSubscribe={handlePaywallSubscribe}
              onDismiss={handlePaywallDismiss}
            />
          )}
        </Show>
      </div>
    </WidgetShell>
  );
}
