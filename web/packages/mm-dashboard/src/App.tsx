import { Suspense } from 'react';
import { BrowserRouter, Routes, Route, Navigate } from 'react-router-dom';
import { ErrorBoundary } from './ErrorBoundary';
import { AdminAuth } from './auth/AdminAuth';
import { Layout } from './components/Layout';
import { APP_BASENAME, STUDIO_ONLY } from './appFlavor';
import { ROUTES } from './routes/manifest';
import { PAGES } from './routes/pages';
import { legacyRouteElements } from './routes/LegacyRedirect';

function PageFallback() {
  return <div className="mm-page-fallback">Loading...</div>;
}

/** The page registered for a route id, behind the lazy-load fallback. */
function PageSlot({ id }: { id: string }) {
  const Page = PAGES[id];
  if (!Page) throw new Error(`No page registered for route "${id}"`);
  return (
    <Suspense fallback={<PageFallback />}>
      <Page />
    </Suspense>
  );
}

/**
 * Every route of the dashboard, built from the route manifest. Legacy redirects are siblings of
 * the <Layout> route, so the Layout guard only ever sees the new path; pages render inside it.
 */
export function AppRoutes() {
  if (STUDIO_ONLY) return <StudioRoutes />;
  return (
    <Routes>
      {legacyRouteElements(ROUTES)}
      <Route element={<Layout />}>
        {ROUTES.map((r) => (
          <Route key={r.id} path={r.path} element={<PageSlot id={r.id} />} />
        ))}
      </Route>
    </Routes>
  );
}

/**
 * Studio-only routes (`/_mm/studio/`): the Creator Studio pages plus
 * mode-agnostic ones. Operator Console routes are not registered at all,
 * so their pages (and their API calls) cannot be reached from this build;
 * anything unknown lands on the Studio home.
 */
export function StudioRoutes() {
  return (
    <Routes>
      {legacyRouteElements(ROUTES)}
      <Route path="/" element={<Navigate to="/creator" replace />} />
      <Route element={<Layout />}>
        {ROUTES.filter((r) => r.mode !== 'operator').map((r) => (
          <Route key={r.id} path={r.path} element={<PageSlot id={r.id} />} />
        ))}
      </Route>
      <Route path="*" element={<Navigate to="/creator" replace />} />
    </Routes>
  );
}

export function App() {
  return (
    <ErrorBoundary>
      <AdminAuth>
        <BrowserRouter basename={APP_BASENAME}>
          <AppRoutes />
        </BrowserRouter>
      </AdminAuth>
    </ErrorBoundary>
  );
}
