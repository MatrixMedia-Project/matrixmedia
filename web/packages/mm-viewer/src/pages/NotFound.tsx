/**
 * Catch-all 404 page for unknown routes.
 */
export function NotFound() {
  return (
    <div className="mm-watch">
      <div className="mm-not-found">
        <h1 className="mm-not-found__title">404</h1>
        <p className="mm-not-found__text">Page not found</p>
        <p className="mm-not-found__text">
          Try a URL like <code>/watch/stream-id</code> or <code>/embed/stream-id</code>
        </p>
      </div>
    </div>
  );
}
