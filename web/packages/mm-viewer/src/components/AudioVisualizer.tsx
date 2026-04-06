import { useRef, useEffect } from 'react';

interface AudioVisualizerProps {
  analyserNode: AnalyserNode | null;
  active: boolean;
}

const BAR_COLOR = '#6366f1';
const BAR_GAP = 2;
const MIN_BAR_WIDTH = 3;

/**
 * Canvas-based frequency bar visualization.
 *
 * Draws frequency bars at ~30fps via requestAnimationFrame with frame skip.
 * Falls back to a static "audio active" indicator if no analyser is available.
 * Respects prefers-reduced-motion by showing a simpler level meter.
 *
 * Ported from mm-widget's SolidJS AudioVisualizer to React.
 */
export function AudioVisualizer({ analyserNode, active }: AudioVisualizerProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const animFrameRef = useRef<number | null>(null);
  const frameCountRef = useRef(0);
  const prefersReducedMotion = useRef(
    window.matchMedia('(prefers-reduced-motion: reduce)').matches,
  );

  // -------------------------------------------------------------------------
  // Drawing functions
  // -------------------------------------------------------------------------

  function drawFrequencyBars(
    ctx: CanvasRenderingContext2D,
    w: number,
    h: number,
    data: Uint8Array,
    bufferLength: number,
  ) {
    const barCount = Math.max(8, Math.floor(w / (MIN_BAR_WIDTH + BAR_GAP)));
    const barWidth = (w - (barCount - 1) * BAR_GAP) / barCount;
    const step = Math.floor(bufferLength / barCount);

    for (let i = 0; i < barCount; i++) {
      const idx = Math.min(i * step, bufferLength - 1);
      const value = (data[idx] ?? 0) / 255;
      const barHeight = Math.max(2, value * h);

      ctx.fillStyle = BAR_COLOR;
      ctx.globalAlpha = 0.6 + value * 0.4;
      ctx.fillRect(i * (barWidth + BAR_GAP), h - barHeight, barWidth, barHeight);
    }
    ctx.globalAlpha = 1;
  }

  function drawLevelMeter(
    ctx: CanvasRenderingContext2D,
    w: number,
    h: number,
    data: Uint8Array,
  ) {
    let sum = 0;
    for (let i = 0; i < data.length; i++) sum += (data[i] ?? 0);
    const avg = sum / data.length / 255;
    const barWidth = avg * w;

    ctx.fillStyle = BAR_COLOR;
    ctx.globalAlpha = 0.8;
    ctx.fillRect(0, h / 2 - 4, barWidth, 8);
    ctx.globalAlpha = 1;
  }

  function drawStaticBars(ctx: CanvasRenderingContext2D, w: number, h: number) {
    const barCount = Math.max(8, Math.floor(w / (MIN_BAR_WIDTH + BAR_GAP)));
    const barWidth = (w - (barCount - 1) * BAR_GAP) / barCount;

    for (let i = 0; i < barCount; i++) {
      const height = ((Math.sin(i * 0.7 + Date.now() * 0.002) + 1) / 2) * h * 0.6 + h * 0.1;

      ctx.fillStyle = BAR_COLOR;
      ctx.globalAlpha = 0.4;
      ctx.fillRect(i * (barWidth + BAR_GAP), h - height, barWidth, height);
    }
    ctx.globalAlpha = 1;
  }

  // -------------------------------------------------------------------------
  // ResizeObserver for canvas DPI scaling
  // -------------------------------------------------------------------------

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const { width, height } = entry.contentRect;
        canvas.width = width * window.devicePixelRatio;
        canvas.height = height * window.devicePixelRatio;
        const ctx = canvas.getContext('2d');
        if (ctx) ctx.scale(window.devicePixelRatio, window.devicePixelRatio);
        canvas.style.width = `${width}px`;
        canvas.style.height = `${height}px`;
      }
    });

    observer.observe(canvas);
    return () => observer.disconnect();
  }, []);

  // -------------------------------------------------------------------------
  // Animation loop
  // -------------------------------------------------------------------------

  useEffect(() => {
    if (!active) {
      // Stop animation and clear canvas
      if (animFrameRef.current) {
        cancelAnimationFrame(animFrameRef.current);
        animFrameRef.current = null;
      }
      const canvas = canvasRef.current;
      if (canvas) {
        const ctx = canvas.getContext('2d');
        if (ctx) ctx.clearRect(0, 0, canvas.width, canvas.height);
      }
      return;
    }

    frameCountRef.current = 0;

    function animate() {
      frameCountRef.current++;
      // ~30fps: skip every other frame on 60fps displays
      if (frameCountRef.current % 2 === 0) {
        const canvas = canvasRef.current;
        if (!canvas) return;
        const ctx = canvas.getContext('2d');
        if (!ctx) return;

        const w = canvas.width / window.devicePixelRatio;
        const h = canvas.height / window.devicePixelRatio;
        ctx.clearRect(0, 0, canvas.width, canvas.height);

        if (!analyserNode) {
          drawStaticBars(ctx, w, h);
        } else {
          const bufferLength = analyserNode.frequencyBinCount;
          const dataArray = new Uint8Array(bufferLength);
          analyserNode.getByteFrequencyData(dataArray);

          if (prefersReducedMotion.current) {
            drawLevelMeter(ctx, w, h, dataArray);
          } else {
            drawFrequencyBars(ctx, w, h, dataArray, bufferLength);
          }
        }
      }
      animFrameRef.current = requestAnimationFrame(animate);
    }

    animate();

    return () => {
      if (animFrameRef.current) {
        cancelAnimationFrame(animFrameRef.current);
        animFrameRef.current = null;
      }
    };
  }, [active, analyserNode]);

  return (
    <div className={`mm-visualizer ${!active ? 'mm-visualizer--static' : ''}`}>
      {active ? (
        <canvas ref={canvasRef} />
      ) : (
        <span className="mm-visualizer__idle">No active stream</span>
      )}
    </div>
  );
}
