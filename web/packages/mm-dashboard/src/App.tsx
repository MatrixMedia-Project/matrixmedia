import { lazy, Suspense } from 'react';
import { BrowserRouter, Routes, Route } from 'react-router-dom';
import { AdminAuth } from './auth/AdminAuth';
import { Layout } from './components/Layout';
import { Overview } from './pages/Overview';

// Lazy-load secondary admin pages so the initial shell only ships the
// Overview route. Each page is fetched on first navigation.
const Streams = lazy(() =>
  import('./pages/Streams').then((m) => ({ default: m.Streams })),
);
const StreamDetail = lazy(() =>
  import('./pages/StreamDetail').then((m) => ({ default: m.StreamDetail })),
);
const Recordings = lazy(() =>
  import('./pages/Recordings').then((m) => ({ default: m.Recordings })),
);
const Config = lazy(() =>
  import('./pages/Config').then((m) => ({ default: m.Config })),
);
const Settings = lazy(() =>
  import('./pages/Settings').then((m) => ({ default: m.Settings })),
);
const Logs = lazy(() =>
  import('./pages/Logs').then((m) => ({ default: m.Logs })),
);
const Users = lazy(() =>
  import('./pages/Users').then((m) => ({ default: m.Users })),
);

function PageFallback() {
  return <div className="mm-page-fallback">Loading...</div>;
}

export function App() {
  return (
    <AdminAuth>
      <BrowserRouter basename="/_mm/dashboard">
        <Routes>
          <Route element={<Layout />}>
            <Route index element={<Overview />} />
            <Route
              path="streams"
              element={
                <Suspense fallback={<PageFallback />}>
                  <Streams />
                </Suspense>
              }
            />
            <Route
              path="streams/:id"
              element={
                <Suspense fallback={<PageFallback />}>
                  <StreamDetail />
                </Suspense>
              }
            />
            <Route
              path="recordings"
              element={
                <Suspense fallback={<PageFallback />}>
                  <Recordings />
                </Suspense>
              }
            />
            <Route
              path="config"
              element={
                <Suspense fallback={<PageFallback />}>
                  <Config />
                </Suspense>
              }
            />
            <Route
              path="settings"
              element={
                <Suspense fallback={<PageFallback />}>
                  <Settings />
                </Suspense>
              }
            />
            <Route
              path="logs"
              element={
                <Suspense fallback={<PageFallback />}>
                  <Logs />
                </Suspense>
              }
            />
            <Route
              path="users"
              element={
                <Suspense fallback={<PageFallback />}>
                  <Users />
                </Suspense>
              }
            />
          </Route>
        </Routes>
      </BrowserRouter>
    </AdminAuth>
  );
}
