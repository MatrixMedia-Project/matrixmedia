import { lazy, Suspense } from 'react';
import { BrowserRouter, Routes, Route } from 'react-router-dom';
import { NotFound } from './pages/NotFound';

// Lazy-load page components so each route only fetches the code it needs.
// This keeps the initial chunk small (no LiveKit, no HLS.js) until the
// user actually navigates to a page that requires them.
const WatchPage = lazy(() =>
  import('./pages/WatchPage').then((m) => ({ default: m.WatchPage })),
);
const EmbedPage = lazy(() =>
  import('./pages/EmbedPage').then((m) => ({ default: m.EmbedPage })),
);
const RecordingPage = lazy(() =>
  import('./pages/RecordingPage').then((m) => ({ default: m.RecordingPage })),
);

function PageFallback() {
  return <div className="mm-page-fallback">Loading...</div>;
}

export function App() {
  return (
    <BrowserRouter basename="/_mm/viewer">
      <Suspense fallback={<PageFallback />}>
        <Routes>
          <Route path="/watch/:streamId" element={<WatchPage />} />
          <Route path="/embed/:streamId" element={<EmbedPage />} />
          <Route path="/recording/:recordingId" element={<RecordingPage />} />
          <Route path="*" element={<NotFound />} />
        </Routes>
      </Suspense>
    </BrowserRouter>
  );
}
