import { createEffect, onCleanup, onMount } from 'solid-js';

interface AudioVisualizerProps {
  analyserNode: AnalyserNode | null;
  active: boolean;
}

/**
 * Canvas-based frequency bar visualization.
 *
 * Draws frequency bars at ~30fps via requestAnimationFrame with frame skip.
 * Falls back to a static "audio active" indicator if no analyser is available.
 * Respects prefers-reduced-motion by showing a simpler level meter.
 */
export function AudioVisualizer(props: AudioVisualizerProps) {
  let canvasRef: HTMLCanvasElement | undefined;
  let animFrameId: number | null = null;
  let frameCount = 0;
  const prefersReducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  const BAR_COLOR = '#6366f1';
  const BAR_GAP = 2;
  const MIN_BAR_WIDTH = 3;

  function draw() {
    if (!canvasRef) return;

    const ctx = canvasRef.getContext('2d');
    if (!ctx) return;

    const w = canvasRef.width;
    const h = canvasRef.height;

    ctx.clearRect(0, 0, w, h);

    if (!props.active) {
      // Idle state -- just clear
      return;
    }

    if (!props.analyserNode) {
      // No analyser -- draw static indicator bars
      drawStaticBars(ctx, w, h);
      return;
    }

    const analyser = props.analyserNode;
    const bufferLength = analyser.frequencyBinCount;
    const dataArray = new Uint8Array(bufferLength);
    analyser.getByteFrequencyData(dataArray);

    if (prefersReducedMotion) {
      // Reduced motion: single level meter
      drawLevelMeter(ctx, w, h, dataArray);
    } else {
      drawFrequencyBars(ctx, w, h, dataArray, bufferLength);
    }
  }

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
      const value = data[idx] / 255;
      const barHeight = Math.max(2, value * h);

      ctx.fillStyle = BAR_COLOR;
      ctx.globalAlpha = 0.6 + value * 0.4;
      ctx.fillRect(
        i * (barWidth + BAR_GAP),
        h - barHeight,
        barWidth,
        barHeight,
      );
    }
    ctx.globalAlpha = 1;
  }

  function drawLevelMeter(
    ctx: CanvasRenderingContext2D,
    w: number,
    h: number,
    data: Uint8Array,
  ) {
    // Average level
    let sum = 0;
    for (let i = 0; i < data.length; i++) sum += data[i];
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
      // Pseudo-random but deterministic heights for visual interest
      const height = ((Math.sin(i * 0.7 + Date.now() * 0.002) + 1) / 2) * h * 0.6 + h * 0.1;

      ctx.fillStyle = BAR_COLOR;
      ctx.globalAlpha = 0.4;
      ctx.fillRect(
        i * (barWidth + BAR_GAP),
        h - height,
        barWidth,
        height,
      );
    }
    ctx.globalAlpha = 1;
  }

  function animate() {
    frameCount++;
    // ~30fps: skip every other frame on 60fps displays
    if (frameCount % 2 === 0) {
      draw();
    }
    animFrameId = requestAnimationFrame(animate);
  }

  onMount(() => {
    if (!canvasRef) return;

    const resizeObserver = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const { width, height } = entry.contentRect;
        canvasRef!.width = width * window.devicePixelRatio;
        canvasRef!.height = height * window.devicePixelRatio;
        const ctx = canvasRef!.getContext('2d');
        if (ctx) ctx.scale(window.devicePixelRatio, window.devicePixelRatio);
        // Re-set logical dimensions for CSS
        canvasRef!.style.width = `${width}px`;
        canvasRef!.style.height = `${height}px`;
      }
    });

    resizeObserver.observe(canvasRef);
    onCleanup(() => resizeObserver.disconnect());
  });

  createEffect(() => {
    // Start/stop animation based on active state
    if (props.active) {
      if (!animFrameId) {
        frameCount = 0;
        animate();
      }
    } else {
      if (animFrameId) {
        cancelAnimationFrame(animFrameId);
        animFrameId = null;
      }
      // Clear canvas when inactive
      if (canvasRef) {
        const ctx = canvasRef.getContext('2d');
        if (ctx) ctx.clearRect(0, 0, canvasRef.width, canvasRef.height);
      }
    }
  });

  onCleanup(() => {
    if (animFrameId) cancelAnimationFrame(animFrameId);
  });

  return (
    <div class={`mm-visualizer ${!props.active ? 'mm-visualizer--static' : ''}`}>
      {props.active ? (
        <canvas ref={canvasRef} />
      ) : (
        <span>No active stream</span>
      )}
    </div>
  );
}
