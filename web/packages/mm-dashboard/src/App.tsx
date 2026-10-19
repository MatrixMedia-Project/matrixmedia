import { Suspense } from 'react';
import { BrowserRouter, Routes, Route } from 'react-router-dom';
import { ErrorBoundary } from './ErrorBoundary';
import { AdminAuth } from './auth/AdminAuth';
import { Layout } from './components/Layout';
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

export function App() {
  return (
    <ErrorBoundary>
      <AdminAuth>
        <BrowserRouter basename="/_mm/dashboard">
          <AppRoutes />
        </BrowserRouter>
      </AdminAuth>
    </ErrorBoundary>
  );
}
