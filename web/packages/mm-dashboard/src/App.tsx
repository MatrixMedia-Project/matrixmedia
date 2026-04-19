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
const Subscriptions = lazy(() =>
  import('./pages/Subscriptions').then((m) => ({ default: m.Subscriptions })),
);
const ContentGates = lazy(() =>
  import('./pages/ContentGates').then((m) => ({ default: m.ContentGates })),
);
const SwitchLab = lazy(() =>
  import('./pages/SwitchLab').then((m) => ({ default: m.SwitchLab })),
);
const Donations = lazy(() =>
  import('./pages/Donations').then((m) => ({ default: m.Donations })),
);
const Creators = lazy(() =>
  import('./pages/Creators').then((m) => ({ default: m.Creators })),
);
const Ads = lazy(() =>
  import('./pages/Ads').then((m) => ({ default: m.Ads })),
);
const MyTiers = lazy(() =>
  import('./pages/MyTiers').then((m) => ({ default: m.MyTiers })),
);
const MyDefaults = lazy(() =>
  import('./pages/MyDefaults').then((m) => ({ default: m.MyDefaults })),
);
const MyEarnings = lazy(() =>
  import('./pages/MyEarnings').then((m) => ({ default: m.MyEarnings })),
);
const MySubscribers = lazy(() =>
  import('./pages/MySubscribers').then((m) => ({ default: m.MySubscribers })),
);
const MyRooms = lazy(() =>
  import('./pages/MyRooms').then((m) => ({ default: m.MyRooms })),
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
            <Route
              path="subscriptions"
              element={
                <Suspense fallback={<PageFallback />}>
                  <Subscriptions />
                </Suspense>
              }
            />
            <Route
              path="content-gates"
              element={
                <Suspense fallback={<PageFallback />}>
                  <ContentGates />
                </Suspense>
              }
            />
            <Route
              path="donations"
              element={
                <Suspense fallback={<PageFallback />}>
                  <Donations />
                </Suspense>
              }
            />
            <Route
              path="creators"
              element={
                <Suspense fallback={<PageFallback />}>
                  <Creators />
                </Suspense>
              }
            />
            <Route
              path="ads"
              element={
                <Suspense fallback={<PageFallback />}>
                  <Ads />
                </Suspense>
              }
            />
            <Route
              path="switch-lab"
              element={
                <Suspense fallback={<PageFallback />}>
                  <SwitchLab />
                </Suspense>
              }
            />
            <Route
              path="creator/tiers"
              element={
                <Suspense fallback={<PageFallback />}>
                  <MyTiers />
                </Suspense>
              }
            />
            <Route
              path="creator/defaults"
              element={
                <Suspense fallback={<PageFallback />}>
                  <MyDefaults />
                </Suspense>
              }
            />
            <Route
              path="creator/earnings"
              element={
                <Suspense fallback={<PageFallback />}>
                  <MyEarnings />
                </Suspense>
              }
            />
            <Route
              path="creator/subscribers"
              element={
                <Suspense fallback={<PageFallback />}>
                  <MySubscribers />
                </Suspense>
              }
            />
            <Route
              path="creator/rooms"
              element={
                <Suspense fallback={<PageFallback />}>
                  <MyRooms />
                </Suspense>
              }
            />
          </Route>
        </Routes>
      </BrowserRouter>
    </AdminAuth>
  );
}
