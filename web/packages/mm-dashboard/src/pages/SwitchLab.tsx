// Switch Lab — vanilla per-viewer WebRTC test against mm-switch.
// Mirrors the validated prototype at ~/Documents/mm-switch-lab/.
// Three sources (Camera, Stream, Movie); independent viewer channels.

import { useEffect, useRef, useState } from 'react';

const DEFAULT_SWITCH_URL = `${location.protocol}//${location.host}/_mm/switch`;
// Bundled with the dashboard build — same-origin so captureStream() works
// without CORS issues.
const DEFAULT_STREAM_URL = '/_mm/dashboard/lab-media/stream.webm';
const DEFAULT_MOVIE_URL = '/_mm/dashboard/lab-media/bunny.webm';
const ICE: RTCConfiguration = { iceServers: [] };

type LogType = 'info' | 'ok' | 'err' | 'warn';
interface LogEntry { n: number; ts: string; msg: string; type: LogType }

interface ViewerSlot {
  id: string;
  pc: RTCPeerConnection;
  viewerId: string;
  sourceId: string;
  returnSource?: string;
  adPC?: RTCPeerConnection;
  adVideo?: HTMLVideoElement;
  personalSourceId?: string;
  statsTimer: number;
}

export function SwitchLab() {
  const [switchUrl, setSwitchUrl] = useState(DEFAULT_SWITCH_URL);
  const [streamUrl, setStreamUrl] = useState(DEFAULT_STREAM_URL);
  const [movieUrl, setMovieUrl] = useState(DEFAULT_MOVIE_URL);
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [, setTick] = useState(0);

  const camStreamRef = useRef<MediaStream | null>(null);
  const camPCRef = useRef<RTCPeerConnection | null>(null);
  const s2PCRef = useRef<RTCPeerConnection | null>(null);
  const camVideoRef = useRef<HTMLVideoElement>(null);
  const s2VideoRef = useRef<HTMLVideoElement>(null);
  const movieVideoRef = useRef<HTMLVideoElement>(null);
  const viewersRef = useRef<Map<string, ViewerSlot>>(new Map());
  const slotCounterRef = useRef(0);
  const logCounterRef = useRef(0);

  const [camOn, setCamOn] = useState(false);
  const [s2On, setS2On] = useState(false);
  // React state mirror of the viewers Map — drives panel rendering.
  // Panels appear BEFORE connectViewer fires, so the <video> element exists
  // by the time ontrack tries to attach the stream.
  const [viewerIds, setViewerIds] = useState<string[]>([]);

  const log = (msg: string, type: LogType = 'info') => {
    logCounterRef.current += 1;
    const now = new Date();
    const ts = `${now.toLocaleTimeString()}.${String(now.getMilliseconds()).padStart(3, '0')}`;
    setLogs((prev) => [{ n: logCounterRef.current, ts, msg, type }, ...prev].slice(0, 500));
  };

  const api = async (method: string, path: string, body?: unknown) => {
    const opts: RequestInit = { method, headers: { 'Content-Type': 'application/json' } };
    if (body) opts.body = JSON.stringify(body);
    const resp = await fetch(switchUrl.replace(/\/$/, '') + path, opts);
    if (!resp.ok) throw new Error(`${resp.status} ${await resp.text()}`);
    return resp.json();
  };

  const waitGather = (pc: RTCPeerConnection): Promise<void> =>
    new Promise((resolve) => {
      if (pc.iceGatheringState === 'complete') return resolve();
      const t = window.setTimeout(() => resolve(), 500);
      pc.onicegatheringstatechange = () => {
        if (pc.iceGatheringState === 'complete') {
          window.clearTimeout(t);
          resolve();
        }
      };
    });

  const publishStream = async (id: string, stream: MediaStream): Promise<RTCPeerConnection> => {
    const pc = new RTCPeerConnection(ICE);
    stream.getTracks().forEach((t) => pc.addTrack(t, stream));
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    await waitGather(pc);
    const resp = await api('POST', '/api/publish/offer', { id, offer: pc.localDescription });
    await pc.setRemoteDescription(resp.answer);
    return pc;
  };

  const startCamera = async () => {
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ video: true, audio: true });
      camStreamRef.current = stream;
      if (camVideoRef.current) camVideoRef.current.srcObject = stream;
      camPCRef.current = await publishStream('camera', stream);
      setCamOn(true);
      log('Camera published as "camera"', 'ok');
    } catch (e) {
      log(`Camera error: ${(e as Error).message}`, 'err');
    }
  };

  const stopCamera = () => {
    camStreamRef.current?.getTracks().forEach((t) => t.stop());
    camPCRef.current?.close();
    camPCRef.current = null;
    camStreamRef.current = null;
    if (camVideoRef.current) camVideoRef.current.srcObject = null;
    setCamOn(false);
  };

  const startStream = async () => {
    try {
      const v = s2VideoRef.current;
      if (!v) return;
      v.src = streamUrl;
      await v.play();
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const stream: MediaStream = (v as any).captureStream();
      s2PCRef.current = await publishStream('stream2', stream);
      setS2On(true);
      log('Stream published as "stream2"', 'ok');
    } catch (e) {
      log(`Stream error: ${(e as Error).message}`, 'err');
    }
  };

  const stopStream = () => {
    s2VideoRef.current?.pause();
    s2PCRef.current?.close();
    s2PCRef.current = null;
    setS2On(false);
  };

  const previewMovie = async () => {
    const v = movieVideoRef.current;
    if (!v) return;
    if (v.src !== movieUrl) v.src = movieUrl;
    v.currentTime = 0;
    await v.play();
  };

  const stopPersonalMovie = async (slotId: string) => {
    const v = viewersRef.current.get(slotId);
    if (!v) return;
    v.adPC?.close();
    v.adPC = undefined;
    if (v.adVideo) {
      try { v.adVideo.pause(); v.adVideo.remove(); } catch { /* ignore */ }
      v.adVideo = undefined;
    }
    if (v.personalSourceId) {
      try { await api('DELETE', `/api/sources/${v.personalSourceId}`); } catch { /* ignore */ }
      v.personalSourceId = undefined;
    }
  };

  const playPersonalMovie = async (slotId: string) => {
    const v = viewersRef.current.get(slotId);
    if (!v) { log(`${slotId} not connected`, 'warn'); return; }
    await stopPersonalMovie(slotId);

    const personalSourceId = `movie-${slotId}-${Date.now()}`;
    const video = document.createElement('video');
    video.src = movieUrl;
    video.muted = true;
    video.playsInline = true;
    video.style.display = 'none';
    document.body.appendChild(video);
    await video.play();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const stream: MediaStream = (video as any).captureStream();

    const adPC = await publishStream(personalSourceId, stream);
    v.returnSource = v.sourceId;
    v.adPC = adPC;
    v.adVideo = video;
    v.personalSourceId = personalSourceId;
    await switchViewer(slotId, personalSourceId);
    log(`${slotId} playing personal movie from 0`, 'ok');

    video.addEventListener('ended', async () => {
      log(`${slotId} movie finished`, 'info');
      const vNow = viewersRef.current.get(slotId);
      if (vNow && vNow.sourceId === personalSourceId && vNow.returnSource) {
        try { await switchViewer(slotId, vNow.returnSource); } catch { /* ignore */ }
      }
      await stopPersonalMovie(slotId);
    }, { once: true });
  };

  const switchViewer = async (slotId: string, sourceId: string) => {
    const v = viewersRef.current.get(slotId);
    if (!v) { log(`${slotId} not connected`, 'warn'); return; }
    try {
      await api('POST', '/api/switch', { viewer_id: v.viewerId, source_id: sourceId });
      v.sourceId = sourceId;
      setTick((t) => t + 1);
      log(`${slotId} switched → ${sourceId}`, 'ok');
    } catch (e) {
      log(`${slotId} switch error: ${(e as Error).message}`, 'err');
    }
  };

  const removeViewer = async (slotId: string) => {
    await stopPersonalMovie(slotId);
    const v = viewersRef.current.get(slotId);
    if (v) {
      window.clearInterval(v.statsTimer);
      try { v.pc.close(); } catch { /* ignore */ }
      viewersRef.current.delete(slotId);
    }
    setViewerIds((ids) => ids.filter((id) => id !== slotId));
    log(`${slotId} removed`, 'info');
  };

  const reconnectViewer = async (slotId: string) => {
    const v = viewersRef.current.get(slotId);
    const sourceId = v ? v.sourceId : 'camera';
    if (v) {
      window.clearInterval(v.statsTimer);
      try { v.pc.close(); } catch { /* ignore */ }
      viewersRef.current.delete(slotId);
    }
    const el = document.getElementById(`video-${slotId}`) as HTMLVideoElement | null;
    if (el?.srcObject) {
      (el.srcObject as MediaStream).getTracks().forEach((t) => t.stop());
      el.srcObject = null;
    }
    log(`${slotId} reconnecting → ${sourceId}`, 'info');
    await connectViewer(slotId, sourceId);
  };

  const connectViewer = async (slotId: string, sourceId: string) => {
    log(`${slotId} connecting → ${sourceId}`, 'info');
    try {
      const pc = new RTCPeerConnection(ICE);
      pc.ontrack = (e) => {
        const el = document.getElementById(`video-${slotId}`) as HTMLVideoElement | null;
        if (!el) return;
        if (!el.srcObject) el.srcObject = new MediaStream();
        (el.srcObject as MediaStream).addTrack(e.track);
        el.play().catch((err) => log(`${slotId} play error: ${err.message}`, 'err'));
      };
      pc.onconnectionstatechange = () => {
        log(`${slotId} state: ${pc.connectionState}`, pc.connectionState === 'failed' ? 'err' : 'info');
        setTick((t) => t + 1);
      };
      pc.addTransceiver('video', { direction: 'recvonly' });
      pc.addTransceiver('audio', { direction: 'recvonly' });

      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      await waitGather(pc);
      const resp = await api('POST', '/api/viewers/offer', { offer: pc.localDescription, source_id: sourceId });
      await pc.setRemoteDescription(resp.answer);

      const statsTimer = window.setInterval(() => updateStats(slotId, pc), 1000);
      viewersRef.current.set(slotId, { id: slotId, pc, viewerId: resp.id, sourceId, statsTimer });
      log(`${slotId} → ${resp.id} (${sourceId})`, 'ok');
      setTick((t) => t + 1);
    } catch (e) {
      log(`${slotId} connect failed: ${(e as Error).message}`, 'err');
    }
  };

  const addViewer = () => {
    const slotId = `v${slotCounterRef.current++}`;
    // Render panel first so the <video> element exists before ontrack fires
    setViewerIds((ids) => [...ids, slotId]);
    // Wait for React to commit the DOM, then start the connection
    requestAnimationFrame(() => {
      setTimeout(() => connectViewer(slotId, 'camera'), 0);
    });
  };

  const updateStats = async (slotId: string, pc: RTCPeerConnection) => {
    try {
      const stats = await pc.getStats();
      let bytesIn = 0, pktsIn = 0, framesDecoded = 0, framesDropped = 0, fps = 0, w = 0, h = 0;
      stats.forEach((r) => {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const a = r as any;
        if (a.type === 'inbound-rtp' && a.kind === 'video') {
          bytesIn = a.bytesReceived ?? 0;
          pktsIn = a.packetsReceived ?? 0;
          framesDecoded = a.framesDecoded ?? 0;
          framesDropped = a.framesDropped ?? 0;
          fps = a.framesPerSecond ?? 0;
        }
        if (a.type === 'track' && a.kind === 'video') {
          w = a.frameWidth ?? 0;
          h = a.frameHeight ?? 0;
        }
      });
      const el = document.getElementById(`stats-${slotId}`);
      if (el) {
        const decClass = framesDecoded > 0 ? '#2ecc71' : (pktsIn > 0 ? '#e74c3c' : '#888');
        el.innerHTML = `<span>${(bytesIn / 1024).toFixed(0)} KB</span> ` +
          `<span>${pktsIn} pkts</span> ` +
          `<span style="color:${decClass}">${framesDecoded} dec</span> ` +
          `<span>${framesDropped} drop</span> ` +
          `<span>${fps} fps</span>` +
          (w ? ` <span>${w}×${h}</span>` : '');
      }
    } catch { /* ignore */ }
  };

  // Cleanup on unmount
  useEffect(() => {
    return () => {
      stopCamera();
      stopStream();
      viewersRef.current.forEach((v) => {
        window.clearInterval(v.statsTimer);
        v.pc.close();
        v.adPC?.close();
      });
      viewersRef.current.clear();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div>
      <div className="page-header">
        <h1>Switch Lab</h1>
        <p>Vanilla per-viewer WebRTC test against mm-switch. Each viewer is an independent outgoing channel.</p>
      </div>

      <div className="card" style={{ marginBottom: 12 }}>
        <h3 style={{ marginBottom: 8 }}>Configuration</h3>
        <div style={{ display: 'grid', gridTemplateColumns: '1fr', gap: 8 }}>
          <label style={{ fontSize: 12 }}>
            mm-switch URL
            <input value={switchUrl} onChange={(e) => setSwitchUrl(e.target.value)}
              style={inputStyle} />
          </label>
          <label style={{ fontSize: 12 }}>
            Stream source URL (mp4/webm)
            <input value={streamUrl} onChange={(e) => setStreamUrl(e.target.value)}
              style={inputStyle} />
          </label>
          <label style={{ fontSize: 12 }}>
            Movie source URL (mp4/webm)
            <input value={movieUrl} onChange={(e) => setMovieUrl(e.target.value)}
              style={inputStyle} />
          </label>
        </div>
      </div>

      <h2 style={sectionStyle}>Sources</h2>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 12, marginBottom: 16 }}>
        <div className="card">
          <h3 style={{ marginBottom: 8 }}>
            <Dot on={camOn} /> Camera <Tag>camera</Tag>
          </h3>
          <video ref={camVideoRef} autoPlay muted playsInline style={videoStyle} />
          <div style={barStyle}>
            <button className="btn btn-primary" onClick={startCamera}>Start Camera</button>
            <button className="btn btn-ghost" onClick={stopCamera}>Stop</button>
          </div>
        </div>

        <div className="card">
          <h3 style={{ marginBottom: 8 }}>
            <Dot on={s2On} /> Stream <Tag>stream2</Tag>
          </h3>
          <video ref={s2VideoRef} muted playsInline loop crossOrigin="anonymous" style={videoStyle} />
          <div style={barStyle}>
            <button className="btn btn-primary" onClick={startStream}>Start Stream</button>
            <button className="btn btn-ghost" onClick={stopStream}>Stop</button>
          </div>
        </div>

        <div className="card">
          <h3 style={{ marginBottom: 8 }}>
            <Dot on={true} /> Movie <Tag>per-viewer</Tag>
          </h3>
          <video ref={movieVideoRef} muted playsInline crossOrigin="anonymous" style={videoStyle} />
          <div style={barStyle}>
            <button className="btn btn-primary" onClick={previewMovie}>▶ Preview</button>
            <span style={{ fontSize: 11, color: '#888', marginLeft: 8 }}>Each viewer plays from 0</span>
          </div>
        </div>
      </div>

      <h2 style={sectionStyle}>Viewers (independent outgoing channels)</h2>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(320px, 1fr))', gap: 12 }}>
        {viewerIds.map((slotId) => {
          const v = viewersRef.current.get(slotId);
          const connected = v?.pc?.connectionState === 'connected';
          const sourceLabel = v?.sourceId ?? 'connecting...';
          return (
            <div key={slotId} className="card">
              <h3 style={{ marginBottom: 8 }}>
                <Dot on={connected} /> Viewer {slotId} <Tag>{sourceLabel}</Tag>
              </h3>
              <video id={`video-${slotId}`} autoPlay muted playsInline style={videoStyle} />
              <div style={barStyle}>
                <button className="btn btn-primary" onClick={() => switchViewer(slotId, 'camera')}>→ Camera</button>
                <button className="btn btn-primary" onClick={() => switchViewer(slotId, 'stream2')}>→ Stream</button>
                <button className="btn btn-primary" onClick={() => playPersonalMovie(slotId)}>▶ Movie</button>
                <button className="btn btn-ghost" onClick={() => reconnectViewer(slotId)}>↻</button>
                <button className="btn btn-ghost" onClick={() => removeViewer(slotId)}>×</button>
              </div>
              <div id={`stats-${slotId}`} style={statsStyle}>no stats yet</div>
            </div>
          );
        })}
      </div>

      <div style={{ textAlign: 'center', margin: '16px 0' }}>
        <button className="btn btn-primary" onClick={addViewer} style={{ fontSize: 14, padding: '10px 24px' }}>
          + Add Viewer Channel
        </button>
      </div>

      <h2 style={sectionStyle}>Activity Log</h2>
      <div style={logStyle}>
        {logs.length === 0 && <div style={{ color: '#666' }}>No activity yet.</div>}
        {logs.map((l) => (
          <div key={l.n} style={{ borderBottom: '1px solid #15152a', padding: '1px 0' }}>
            <span style={{ color: '#444' }}>#{String(l.n).padStart(4, '0')}</span>{' '}
            <span style={{ color: '#666' }}>[{l.ts}]</span>{' '}
            <span style={{ color: logColor(l.type) }}>{l.msg}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

const inputStyle: React.CSSProperties = {
  width: '100%',
  padding: '6px 8px',
  marginTop: 4,
  background: '#0a0a1a',
  color: '#ddd',
  border: '1px solid #333',
  borderRadius: 4,
  fontFamily: 'monospace',
  fontSize: 12,
};
const sectionStyle: React.CSSProperties = {
  fontSize: 14,
  color: '#888',
  textTransform: 'uppercase',
  letterSpacing: 2,
  margin: '16px 0 8px',
};
const videoStyle: React.CSSProperties = { width: '100%', background: '#000', borderRadius: 4, aspectRatio: '16/9' };
const barStyle: React.CSSProperties = { display: 'flex', gap: 4, marginTop: 8, flexWrap: 'wrap' };
const statsStyle: React.CSSProperties = {
  fontSize: 10,
  color: '#888',
  fontFamily: 'monospace',
  padding: '4px 6px',
  background: '#0a0a1a',
  borderRadius: 3,
  marginTop: 4,
};
const logStyle: React.CSSProperties = {
  background: '#0a0a1a',
  borderRadius: 8,
  padding: 10,
  fontFamily: 'monospace',
  fontSize: 11,
  maxHeight: 320,
  overflowY: 'auto',
  lineHeight: 1.6,
};
const logColor = (t: LogType) =>
  t === 'ok' ? '#2ecc71' : t === 'err' ? '#e74c3c' : t === 'warn' ? '#f39c12' : '#3498db';

function Dot({ on }: { on: boolean }) {
  return (
    <span style={{
      display: 'inline-block',
      width: 8,
      height: 8,
      borderRadius: '50%',
      background: on ? '#2ecc71' : '#555',
      marginRight: 6,
    }} />
  );
}
function Tag({ children }: { children: React.ReactNode }) {
  return (
    <span style={{
      display: 'inline-block',
      padding: '2px 6px',
      borderRadius: 3,
      fontSize: 10,
      fontWeight: 'bold',
      background: '#444',
      marginLeft: 4,
    }}>{children}</span>
  );
}
