import { useEffect, useState, useCallback } from 'react';
import {
  listServerRequests,
  updateServerRequestStatus,
  type ServerRequest,
  type ServerRequestStatus,
} from '../api/AdminApiClient';
import { isAdmin } from '../auth/AdminAuth';
import { PageHeader } from '../components/PageHeader';

const STATUS_OPTIONS: { value: ServerRequestStatus; label: string }[] = [
  { value: 'new', label: 'New' },
  { value: 'contacted', label: 'Contacted' },
  { value: 'provisioned', label: 'Provisioned' },
  { value: 'declined', label: 'Declined' },
];

function statusBadgeStyle(status: ServerRequestStatus): React.CSSProperties {
  switch (status) {
    case 'new':
      return { background: 'rgba(99,102,241,0.2)', color: '#a5b4fc' };
    case 'contacted':
      return { background: 'rgba(245,158,11,0.2)', color: '#fcd34d' };
    case 'provisioned':
      return { background: 'rgba(34,197,94,0.2)', color: '#86efac' };
    case 'declined':
      return { background: 'rgba(107,114,128,0.2)', color: '#9ca3af' };
  }
}

const badgeBase: React.CSSProperties = {
  display: 'inline-block',
  padding: '2px 8px',
  borderRadius: '4px',
  fontSize: '0.75rem',
  fontWeight: 600,
  textTransform: 'capitalize',
};

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

export function ServerRequests() {
  const [requests, setRequests] = useState<ServerRequest[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [updatingId, setUpdatingId] = useState<string | null>(null);

  const fetchData = useCallback(async () => {
    try {
      const data = await listServerRequests();
      // Newest first
      const sorted = [...data].sort(
        (a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime(),
      );
      setRequests(sorted);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load server requests');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void fetchData();
  }, [fetchData]);

  async function onStatusChange(id: string, newStatus: ServerRequestStatus) {
    // Optimistic update
    setRequests((prev) =>
      prev.map((r) => (r.id === id ? { ...r, status: newStatus } : r)),
    );
    setUpdatingId(id);
    try {
      const updated = await updateServerRequestStatus(id, newStatus);
      setRequests((prev) => prev.map((r) => (r.id === id ? updated : r)));
    } catch (err) {
      // Rollback on error
      setError(err instanceof Error ? err.message : 'Status update failed');
      void fetchData();
    } finally {
      setUpdatingId(null);
    }
  }

  if (!isAdmin()) {
    return (
      <div>
        <PageHeader title="Server Requests" description="Inbound server provisioning requests." />
        <div className="card" style={{ textAlign: 'center', padding: '3rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>Admin access required.</p>
        </div>
      </div>
    );
  }

  return (
    <div>
      <PageHeader
        title="Server Requests"
        description="Inbound provisioning requests. Update status as you contact, provision, or decline each."
      />

      {error && (
        <div
          className="card"
          style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}
        >
          {error}
        </div>
      )}

      {loading && requests.length === 0 ? (
        <div className="loading">Loading…</div>
      ) : requests.length === 0 ? (
        <div className="card" style={{ textAlign: 'center', padding: '3rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>No server requests yet.</p>
        </div>
      ) : (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Organisation</th>
                <th>Contact</th>
                <th>Region</th>
                <th>Size</th>
                <th>Domain</th>
                <th>Notes</th>
                <th>Submitted</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>
              {requests.map((req) => (
                <tr key={req.id}>
                  <td style={{ fontWeight: 600 }}>{req.org_name}</td>
                  <td>
                    <a
                      href={`mailto:${req.contact_email}`}
                      style={{ color: 'var(--mm-color-primary)', textDecoration: 'none' }}
                    >
                      {req.contact_email}
                    </a>
                  </td>
                  <td>{req.region}</td>
                  <td>{req.instance_size}</td>
                  <td>
                    <span className="truncate" title={req.domain ?? ''}>
                      {req.domain || <span style={{ color: 'var(--mm-color-text-secondary)' }}>—</span>}
                    </span>
                  </td>
                  <td>
                    <span
                      className="truncate"
                      title={req.notes ?? ''}
                      style={{ maxWidth: 180 }}
                    >
                      {req.notes || <span style={{ color: 'var(--mm-color-text-secondary)' }}>—</span>}
                    </span>
                  </td>
                  <td style={{ whiteSpace: 'nowrap', fontSize: '0.8rem' }}>
                    {formatTimestamp(req.created_at)}
                  </td>
                  <td>
                    <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
                      <span style={{ ...badgeBase, ...statusBadgeStyle(req.status) }}>
                        {req.status}
                      </span>
                      <select
                        className="input"
                        value={req.status}
                        disabled={updatingId === req.id}
                        onChange={(e) =>
                          void onStatusChange(req.id, e.target.value as ServerRequestStatus)
                        }
                        style={{
                          padding: '2px 4px',
                          fontSize: '0.75rem',
                          width: 'auto',
                          cursor: 'pointer',
                          minWidth: 110,
                        }}
                        aria-label={`Status for ${req.org_name}`}
                      >
                        {STATUS_OPTIONS.map((opt) => (
                          <option key={opt.value} value={opt.value}>
                            {opt.label}
                          </option>
                        ))}
                      </select>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
