export function Logs() {
  return (
    <div>
      <div className="page-header">
        <h1>Logs</h1>
        <p>
          Structured server logs — streamed live from mm-core. Use <code>docker logs -f mm-core</code> in the meantime.
        </p>
      </div>

      <div className="card placeholder">
        <h2>Log viewer coming in Phase 2</h2>
        <p>
          Server logs are currently available via stdout and Docker logs. Run{' '}
          <code className="mono" style={{ color: 'var(--mm-color-primary)' }}>
            docker logs -f mm-core
          </code>{' '}
          to follow live output. Structured JSON logs are emitted when{' '}
          <code className="mono" style={{ color: 'var(--mm-color-primary)' }}>
            RUST_LOG
          </code>{' '}
          is configured.
        </p>
      </div>
    </div>
  );
}
