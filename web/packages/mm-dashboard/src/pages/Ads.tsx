import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import type {
  AdCreativeInfo,
  AdStatus,
  CreateAdRequest,
  UpdateAdRequest,
  AdStatsResponse,
  AdAnalyticsResponse,
} from '../types';
import {
  listAds,
  createAd,
  updateAd,
  deleteAd,
  uploadAdFile,
  retryAdConversion,
  getAdUploadLimitMb,
  getAdStats,
  getAdAnalytics,
} from '../api/AdminApiClient';
import { isAdmin } from '../auth/AdminAuth';
import {
  anyBusy,
  durationText,
  factLine,
  isBusy,
  oversizeText,
  progressText,
  uploadErrorText,
  warningText,
} from './ads/conversion';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type StatusFilter = 'all' | 'draft' | 'processing' | 'ready' | 'failed' | 'paused';

const STATUS_OPTIONS: StatusFilter[] = ['all', 'draft', 'processing', 'ready', 'failed', 'paused'];

const PLACEMENT_OPTIONS = ['pre_roll', 'mid_roll', 'post_roll', 'any'] as const;

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

function truncateId(id: string, max = 12): string {
  if (id.length <= max) return id;
  return `${id.slice(0, max - 4)}...`;
}

function formatFileSize(bytes: number): string {
  if (bytes <= 0) return '--';
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatPct(value: number): string {
  return `${(value * 100).toFixed(1)}%`;
}

function formatNumber(n: number): string {
  return n.toLocaleString();
}

function statusBadgeClass(status: AdStatus): string {
  switch (status) {
    case 'ready':
      return 'badge badge-active';
    case 'failed':
      return 'badge badge-warning';
    default:
      return 'badge badge-ended';
  }
}

function ownerLabel(ownerType: string, ownerId: string): string {
  if (ownerType === 'platform') return 'Platform';
  return ownerId;
}

function placementLabel(p: string): string {
  return p.replace(/_/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
}

// ---------------------------------------------------------------------------
// Inline styles (following SwitchLab.tsx pattern)
// ---------------------------------------------------------------------------

const inputStyle: React.CSSProperties = {
  width: '100%',
  padding: '6px 8px',
  marginTop: 4,
  background: '#0a0a1a',
  color: '#ddd',
  border: '1px solid #333',
  borderRadius: 4,
  fontSize: '0.875rem',
};

const selectStyle: React.CSSProperties = {
  ...inputStyle,
  cursor: 'pointer',
};

const formGroupStyle: React.CSSProperties = {
  marginBottom: 'var(--mm-space-md)',
};

const labelStyle: React.CSSProperties = {
  display: 'block',
  fontSize: '0.8125rem',
  color: 'var(--mm-color-text-secondary)',
  marginBottom: 4,
};

// ---------------------------------------------------------------------------
// Sub-components
// ---------------------------------------------------------------------------

interface StatsInlineProps {
  stats: AdStatsResponse;
  onClose: () => void;
}

function StatsInline({ stats, onClose }: StatsInlineProps) {
  return (
    <tr>
      <td colSpan={9}>
        <div
          className="card"
          style={{
            margin: 'var(--mm-space-sm) 0',
            display: 'grid',
            gridTemplateColumns: 'repeat(auto-fit, minmax(120px, 1fr))',
            gap: 'var(--mm-space-md)',
            alignItems: 'center',
          }}
        >
          <div>
            <div style={{ fontSize: '1.25rem', fontWeight: 700 }}>
              {formatNumber(stats.total_impressions)}
            </div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.75rem' }}>
              Impressions
            </div>
          </div>
          <div>
            <div style={{ fontSize: '1.25rem', fontWeight: 700 }}>
              {formatNumber(stats.completions)}
            </div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.75rem' }}>
              Completions
            </div>
          </div>
          <div>
            <div style={{ fontSize: '1.25rem', fontWeight: 700 }}>
              {formatNumber(stats.skips)}
            </div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.75rem' }}>
              Skips
            </div>
          </div>
          <div>
            <div style={{ fontSize: '1.25rem', fontWeight: 700 }}>
              {formatNumber(stats.clicks)}
            </div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.75rem' }}>
              Clicks
            </div>
          </div>
          <div>
            <div style={{ fontSize: '1.25rem', fontWeight: 700 }}>
              {formatPct(stats.completion_rate)}
            </div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.75rem' }}>
              Completion Rate
            </div>
          </div>
          <div>
            <div style={{ fontSize: '1.25rem', fontWeight: 700 }}>
              {formatPct(stats.ctr)}
            </div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.75rem' }}>
              CTR
            </div>
          </div>
          <div>
            <button className="btn btn-sm btn-ghost" onClick={onClose}>
              Close
            </button>
          </div>
        </div>
      </td>
    </tr>
  );
}

const secondaryLine: React.CSSProperties = {
  color: 'var(--mm-color-text-secondary)',
  fontSize: '0.8em',
};

/**
 * The Media column: the live preview with its measured facts, then what the newest conversion is
 * doing or why it failed. The two are independent: an ad that is live keeps its preview while a
 * re-upload converts (it plays the old media until the new one is published) or fails.
 */
function MediaCell({ ad }: { ad: AdCreativeInfo }) {
  const job = ad.media?.job ?? null;
  const warnings = job?.warnings ?? [];
  const busy = isBusy(job);
  const failure = job?.error ?? null;
  return (
    <>
      {ad.preview_url && (
        <div>
          <a href={ad.preview_url} target="_blank" rel="noopener noreferrer" style={{ color: 'var(--mm-color-primary)' }}>
            View
          </a>
          {ad.media?.vp8 && <div style={secondaryLine}>{factLine(ad.media.vp8)}</div>}
        </div>
      )}
      {job && busy && (
        <div style={{ color: 'var(--mm-color-text-secondary)' }}>{progressText(job)}</div>
      )}
      {failure && (
        <div>
          <div>{failure.message}</div>
          <div style={secondaryLine}>{failure.hint}</div>
        </div>
      )}
      {warnings.map((w, i) => (
        <div key={`${i}-${w}`} style={secondaryLine}>{warningText(w)}</div>
      ))}
      {!ad.preview_url && !busy && !failure && (
        <span style={{ color: 'var(--mm-color-text-secondary)' }}>No file</span>
      )}
    </>
  );
}

// ---------------------------------------------------------------------------
// Main component
// ---------------------------------------------------------------------------

export function Ads() {
  // Data
  const [ads, setAds] = useState<AdCreativeInfo[]>([]);
  const [analytics, setAnalytics] = useState<AdAnalyticsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');

  // Filters
  const [statusFilter, setStatusFilter] = useState<StatusFilter>('all');

  // Dialogs
  const [showCreate, setShowCreate] = useState(false);
  const [editingAd, setEditingAd] = useState<AdCreativeInfo | null>(null);
  const [uploadTarget, setUploadTarget] = useState<AdCreativeInfo | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<AdCreativeInfo | null>(null);

  // Per-ad stats
  const [statsAdId, setStatsAdId] = useState<string | null>(null);
  const [adStats, setAdStats] = useState<AdStatsResponse | null>(null);
  const [statsLoading, setStatsLoading] = useState(false);

  // Busy flag for mutations
  const [busy, setBusy] = useState(false);

  // Create form
  const [createTitle, setCreateTitle] = useState('');
  const [createPlacement, setCreatePlacement] = useState<string>('pre_roll');
  const [createClickUrl, setCreateClickUrl] = useState('');
  const [createCategories, setCreateCategories] = useState('');
  const [createFile, setCreateFile] = useState<File | null>(null);

  // Edit form
  const [editTitle, setEditTitle] = useState('');
  const [editPlacement, setEditPlacement] = useState<string>('pre_roll');
  const [editClickUrl, setEditClickUrl] = useState('');
  const [editCategories, setEditCategories] = useState('');
  const [editStatus, setEditStatus] = useState<string>('ready');

  // Upload
  const [uploadFile, setUploadFile] = useState<File | null>(null);
  const [uploading, setUploading] = useState(false);
  const [uploadPct, setUploadPct] = useState(0);
  // Shown inside the create and upload dialogs: a page-level error sits behind their overlay.
  const [uploadError, setUploadError] = useState('');

  // Last created ad ID (for post-create UX)
  const [lastCreatedId, setLastCreatedId] = useState<string | null>(null);

  // -----------------------------------------------------------------------
  // Data fetching
  // -----------------------------------------------------------------------

  const fetchData = useCallback(async () => {
    try {
      const [adList, analyticsData] = await Promise.all([listAds(), getAdAnalytics()]);
      setAds(adList);
      setAnalytics(analyticsData);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch ads');
    } finally {
      setLoading(false);
    }
  }, []);

  // Pause auto-refresh when the tab is hidden
  const visibleRef = useRef(true);
  useEffect(() => {
    const onVisibilityChange = () => {
      visibleRef.current = document.visibilityState === 'visible';
    };
    document.addEventListener('visibilitychange', onVisibilityChange);
    return () => document.removeEventListener('visibilitychange', onVisibilityChange);
  }, []);

  useEffect(() => {
    setLoading(true);
    void fetchData();
  }, [fetchData]);

  // Poll faster while a conversion is running or queued. Depend on the boolean, not on `ads`:
  // a new list arrives on every poll, and re-running the effect on it would refetch at once.
  const converting = anyBusy(ads);
  useEffect(() => {
    const interval = setInterval(() => {
      if (visibleRef.current) void fetchData();
    }, converting ? 2_000 : 15_000);
    return () => clearInterval(interval);
  }, [fetchData, converting]);

  // -----------------------------------------------------------------------
  // Filtered list
  // -----------------------------------------------------------------------

  const filteredAds = useMemo(() => {
    if (statusFilter === 'all') return ads;
    return ads.filter((a) => a.status === statusFilter);
  }, [ads, statusFilter]);

  // -----------------------------------------------------------------------
  // Stats toggle
  // -----------------------------------------------------------------------

  const handleToggleStats = useCallback(
    async (adId: string) => {
      if (statsAdId === adId) {
        setStatsAdId(null);
        setAdStats(null);
        return;
      }
      setStatsAdId(adId);
      setStatsLoading(true);
      try {
        const s = await getAdStats(adId);
        setAdStats(s);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to fetch ad stats');
        setStatsAdId(null);
      } finally {
        setStatsLoading(false);
      }
    },
    [statsAdId],
  );

  // -----------------------------------------------------------------------
  // Create
  // -----------------------------------------------------------------------

  const resetCreateForm = useCallback(() => {
    setCreateTitle('');
    setCreatePlacement('pre_roll');
    setCreateClickUrl('');
    setCreateCategories('');
    setCreateFile(null);
    setUploadError('');
    setLastCreatedId(null);
  }, []);

  const handleOpenCreate = useCallback(() => {
    resetCreateForm();
    setShowCreate(true);
  }, [resetCreateForm]);

  const handleCreate = useCallback(async () => {
    if (!createTitle.trim()) return;
    setBusy(true);
    setUploadPct(0);
    setUploadError('');
    try {
      // Refuse an over-size file before creating anything: a refused upload would leave a draft.
      if (createFile) {
        const tooBig = oversizeText(createFile, await getAdUploadLimitMb());
        if (tooBig) {
          setUploadError(tooBig);
          return;
        }
      }
      const req: CreateAdRequest = {
        title: createTitle.trim(),
        placement: createPlacement,
      };
      if (createClickUrl.trim()) req.click_through_url = createClickUrl.trim();
      if (createCategories.trim()) {
        req.categories = createCategories
          .split(',')
          .map((c) => c.trim())
          .filter(Boolean);
      }
      const result = await createAd(req);
      // If a file was selected, upload it immediately after creation
      if (createFile) {
        setMessage('Ad created. Uploading video file…');
        try {
          await uploadAdFile(result.id, createFile, (f) => {
            setUploadPct(Math.round(f * 100));
            setMessage(`Ad created. Uploading video file… ${Math.round(f * 100)}%`);
          });
          setMessage('Uploaded — converting. The list shows progress.');
        } catch (uploadErr) {
          // Server sentences end with a full stop already; do not double it.
          const why = uploadErrorText(uploadErr).replace(/\.+$/, '');
          setMessage(`Ad created as a draft, but the upload failed: ${why}. You can upload again from the list.`);
        }
        // Auto-close dialog after file upload completes
        setShowCreate(false);
        resetCreateForm();
      } else {
        setLastCreatedId(result.id);
      }
      await fetchData();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create ad');
    } finally {
      setBusy(false);
    }
  }, [createTitle, createPlacement, createClickUrl, createCategories, createFile, fetchData]);

  const handleCloseCreate = useCallback(() => {
    setShowCreate(false);
    resetCreateForm();
  }, [resetCreateForm]);

  // After create -- offer to upload
  const handleUploadForNewAd = useCallback(() => {
    if (!lastCreatedId) return;
    const ad = ads.find((a) => a.id === lastCreatedId);
    if (ad) {
      setUploadTarget(ad);
    } else {
      // Ad just created, might not be in the list yet, construct a minimal target
      setUploadTarget({ id: lastCreatedId } as AdCreativeInfo);
    }
    handleCloseCreate();
  }, [lastCreatedId, ads, handleCloseCreate]);

  // -----------------------------------------------------------------------
  // Edit
  // -----------------------------------------------------------------------

  const handleOpenEdit = useCallback((ad: AdCreativeInfo) => {
    setEditingAd(ad);
    setEditTitle(ad.title);
    setEditPlacement(ad.placement);
    setEditClickUrl(ad.click_through_url ?? '');
    setEditCategories(ad.categories.join(', '));
    setEditStatus(ad.status);
  }, []);

  const handleEdit = useCallback(async () => {
    if (!editingAd) return;
    setBusy(true);
    try {
      const updates: UpdateAdRequest = {};
      if (editTitle.trim() !== editingAd.title) updates.title = editTitle.trim();
      if (editPlacement !== editingAd.placement) updates.placement = editPlacement;
      if (editStatus !== editingAd.status) updates.status = editStatus;
      const newClickUrl = editClickUrl.trim() || undefined;
      if (newClickUrl !== (editingAd.click_through_url ?? undefined)) {
        updates.click_through_url = newClickUrl;
      }
      const newCats = editCategories
        .split(',')
        .map((c) => c.trim())
        .filter(Boolean);
      if (JSON.stringify(newCats) !== JSON.stringify(editingAd.categories)) {
        updates.categories = newCats;
      }
      const res = await updateAd(editingAd.id, updates);
      setEditingAd(null);
      // Lifting a pause from an ad with no converted media is a success that does not make it
      // ready: say where it landed rather than assume the status that was asked for.
      if (updates.status === 'ready' && res.status && res.status !== 'ready') {
        setMessage(`Ad ${truncateId(editingAd.id)} is no longer paused, but it is ${res.status}: it has no converted media to play yet.`);
      } else {
        setMessage(`Ad updated: ${truncateId(editingAd.id)}`);
      }
      await fetchData();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update ad');
    } finally {
      setBusy(false);
    }
  }, [editingAd, editTitle, editPlacement, editClickUrl, editCategories, editStatus, fetchData]);

  // -----------------------------------------------------------------------
  // Upload
  // -----------------------------------------------------------------------

  const handleOpenUpload = useCallback((ad: AdCreativeInfo) => {
    setUploadTarget(ad);
    setUploadFile(null);
    setUploadPct(0);
    setUploadError('');
  }, []);

  const handleUpload = useCallback(async () => {
    if (!uploadTarget || !uploadFile) return;
    setUploading(true);
    setUploadPct(0);
    setUploadError('');
    try {
      const tooBig = oversizeText(uploadFile, await getAdUploadLimitMb());
      if (tooBig) {
        setUploadError(tooBig);
        return;
      }
      await uploadAdFile(uploadTarget.id, uploadFile, (f) => setUploadPct(Math.round(f * 100)));
      setUploadTarget(null);
      setUploadFile(null);
      setMessage('Uploaded — converting. The list shows progress.');
      await fetchData();
    } catch (err) {
      setUploadError(uploadErrorText(err));
    } finally {
      setUploading(false);
    }
  }, [uploadTarget, uploadFile, fetchData]);

  const handleCloseUpload = useCallback(() => {
    setUploadTarget(null);
    setUploadFile(null);
    setUploadPct(0);
    setUploadError('');
  }, []);

  // -----------------------------------------------------------------------
  // Retry a failed conversion
  // -----------------------------------------------------------------------

  const handleRetry = useCallback(
    async (adId: string) => {
      setBusy(true);
      try {
        await retryAdConversion(adId);
        setMessage('Retrying the conversion.');
        await fetchData();
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Retry failed');
      } finally {
        setBusy(false);
      }
    },
    [fetchData],
  );

  // -----------------------------------------------------------------------
  // Delete
  // -----------------------------------------------------------------------

  const handleDelete = useCallback(async () => {
    if (!deleteTarget) return;
    setBusy(true);
    try {
      await deleteAd(deleteTarget.id);
      setDeleteTarget(null);
      setMessage(`Ad deleted: ${truncateId(deleteTarget.id)}`);
      // Close stats if this ad was showing stats
      if (statsAdId === deleteTarget.id) {
        setStatsAdId(null);
        setAdStats(null);
      }
      await fetchData();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to delete ad');
    } finally {
      setBusy(false);
    }
  }, [deleteTarget, statsAdId, fetchData]);

  // -----------------------------------------------------------------------
  // Table rows
  // -----------------------------------------------------------------------

  const tableContent = useMemo(() => {
    const rows: React.ReactNode[] = [];
    for (const ad of filteredAds) {
      rows.push(
        <tr key={ad.id}>
          <td>
            <span title={ad.title}>{ad.title}</span>
          </td>
          <td>
            <span className="truncate" title={ad.owner_id}>
              {ownerLabel(ad.owner_type, ad.owner_id)}
            </span>
          </td>
          <td>{placementLabel(ad.placement)}</td>
          <td>{durationText(ad.media?.vp8)}</td>
          <td>
            <span className={statusBadgeClass(ad.status)}>{ad.status}</span>
          </td>
          <td>
            <MediaCell ad={ad} />
          </td>
          <td>
            <span style={{ fontSize: '0.8125rem' }} title={formatFileSize(ad.file_size_bytes)}>
              {formatTimestamp(ad.created_at)}
            </span>
          </td>
          <td>
            <div style={{ display: 'flex', gap: 4, flexWrap: 'wrap' }}>
              <button
                className="btn btn-sm btn-ghost"
                onClick={() => handleToggleStats(ad.id)}
                disabled={statsLoading && statsAdId === ad.id}
              >
                {statsLoading && statsAdId === ad.id ? '...' : 'Stats'}
              </button>
              <button
                className="btn btn-sm btn-ghost"
                onClick={() => handleOpenEdit(ad)}
                disabled={!isAdmin()}
              >
                Edit
              </button>
              <button
                className="btn btn-sm btn-ghost"
                onClick={() => handleOpenUpload(ad)}
                disabled={!isAdmin()}
              >
                Upload
              </button>
              {ad.status !== 'deleted' && ad.media?.job?.error?.retryable && (
                <button
                  className="btn btn-sm btn-ghost"
                  onClick={() => void handleRetry(ad.id)}
                  disabled={!isAdmin() || busy}
                >
                  Retry
                </button>
              )}
              <button
                className="btn btn-sm btn-danger"
                onClick={() => setDeleteTarget(ad)}
                disabled={ad.status === 'deleted' || !isAdmin()}
              >
                Delete
              </button>
            </div>
          </td>
        </tr>,
      );
      // Inline stats row
      if (statsAdId === ad.id && adStats) {
        rows.push(
          <StatsInline
            key={`stats-${ad.id}`}
            stats={adStats}
            onClose={() => {
              setStatsAdId(null);
              setAdStats(null);
            }}
          />,
        );
      }
    }
    return rows;
  }, [filteredAds, statsAdId, adStats, statsLoading, busy, handleToggleStats, handleOpenEdit, handleOpenUpload, handleRetry]);

  // -----------------------------------------------------------------------
  // Skeleton rows for loading state
  // -----------------------------------------------------------------------

  const skeletonRows = useMemo(
    () =>
      [1, 2, 3, 4].map((i) => (
        <tr key={i}>
          {[1, 2, 3, 4, 5, 6, 7, 8].map((j) => (
            <td key={j}>
              <div
                className="skeleton"
                style={{
                  height: '1em',
                  background: 'var(--mm-color-surface-elevated)',
                  borderRadius: '4px',
                  animation: 'pulse 1.5s ease-in-out infinite',
                }}
              />
            </td>
          ))}
        </tr>
      )),
    [],
  );

  // -----------------------------------------------------------------------
  // Render
  // -----------------------------------------------------------------------

  return (
    <div>
      {/* Page header */}
      <div className="page-header" style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', flexWrap: 'wrap', gap: 'var(--mm-space-md)' }}>
        <div>
          <h1>Ads</h1>
          <p>Platform ad creatives — upload, schedule, and track impressions. Auto-refreshes every 15 s, every 2 s while a conversion runs.</p>
        </div>
        <button className="btn btn-primary" onClick={handleOpenCreate} disabled={!isAdmin()}>
          Create Ad
        </button>
      </div>

      {/* Analytics summary cards */}
      {analytics && (
        <div className="card-grid">
          <div className="card" style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}>
            <div style={{ fontSize: '2rem', fontWeight: 700 }}>
              {formatNumber(analytics.total_ads)}
            </div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
              Total Ads
            </div>
          </div>
          <div className="card" style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}>
            <div style={{ fontSize: '2rem', fontWeight: 700 }}>
              {formatNumber(analytics.total_impressions)}
            </div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
              Impressions
            </div>
          </div>
          <div className="card" style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}>
            <div style={{ fontSize: '2rem', fontWeight: 700 }}>
              {formatPct(analytics.completion_rate)}
            </div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
              Completion Rate
            </div>
          </div>
          <div className="card" style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}>
            <div style={{ fontSize: '2rem', fontWeight: 700 }}>
              {formatPct(analytics.ctr)}
            </div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
              CTR
            </div>
          </div>
        </div>
      )}

      {/* Status filter */}
      <div
        style={{
          display: 'flex',
          gap: 'var(--mm-space-sm)',
          marginBottom: 'var(--mm-space-md)',
          flexWrap: 'wrap',
          alignItems: 'center',
        }}
      >
        <span style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
          Filter:
        </span>
        {STATUS_OPTIONS.map((s) => (
          <button
            key={s}
            className={`btn btn-sm ${statusFilter === s ? '' : 'btn-ghost'}`}
            onClick={() => setStatusFilter(s)}
          >
            {s === 'all' ? 'All' : s.charAt(0).toUpperCase() + s.slice(1)}
          </button>
        ))}
      </div>

      {/* Error */}
      {error && (
        <div
          className="card"
          style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}
        >
          {error}
        </div>
      )}

      {/* Success message */}
      {message && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-lg)' }}>
          {message}
        </div>
      )}

      {/* Ad listing table */}
      {loading && !error && ads.length === 0 ? (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Title</th><th>Owner</th><th>Placement</th><th>Duration</th>
                <th>Status</th><th>Media</th><th>Created</th><th>Actions</th>
              </tr>
            </thead>
            <tbody>{skeletonRows}</tbody>
          </table>
        </div>
      ) : filteredAds.length === 0 ? (
        <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>
            {ads.length === 0 ? 'No ads created yet' : 'No ads match the selected filter'}
          </p>
        </div>
      ) : (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Title</th>
                <th>Owner</th>
                <th>Placement</th>
                <th>Duration</th>
                <th>Status</th>
                <th>Media</th>
                <th>Created</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>{tableContent}</tbody>
          </table>
        </div>
      )}

      {/* ---- Create Ad Dialog ---- */}
      {showCreate && (
        <div className="dialog-overlay" onClick={handleCloseCreate}>
          <div className="dialog" style={{ maxWidth: 500 }} onClick={(e) => e.stopPropagation()}>
            <h2>{lastCreatedId ? 'Ad Created' : 'Create Ad'}</h2>

            {lastCreatedId ? (
              <>
                <p>
                  Ad <span className="mono">{truncateId(lastCreatedId)}</span> created
                  successfully. Would you like to upload a media file?
                </p>
                <div className="dialog-actions">
                  <button className="btn btn-ghost" onClick={handleCloseCreate} disabled={busy}>
                    Done
                  </button>
                  <button className="btn btn-primary" onClick={handleUploadForNewAd} disabled={busy}>
                    Upload File
                  </button>
                </div>
              </>
            ) : (
              <>
                <div style={formGroupStyle}>
                  <label style={labelStyle}>Title *</label>
                  <input
                    style={inputStyle}
                    value={createTitle}
                    onChange={(e) => setCreateTitle(e.target.value)}
                    placeholder="Ad title"
                    autoFocus
                  />
                </div>

                <div style={formGroupStyle}>
                  <label style={labelStyle}>Placement</label>
                  <select
                    style={selectStyle}
                    value={createPlacement}
                    onChange={(e) => setCreatePlacement(e.target.value)}
                  >
                    {PLACEMENT_OPTIONS.map((p) => (
                      <option key={p} value={p}>
                        {placementLabel(p)}
                      </option>
                    ))}
                  </select>
                </div>

                <div style={formGroupStyle}>
                  <label style={labelStyle}>Video file (from your computer)</label>
                  <input
                    type="file"
                    accept="video/*"
                    style={{ ...inputStyle, padding: '8px' }}
                    onChange={(e) => {
                      setCreateFile(e.target.files?.[0] ?? null);
                      setUploadError('');
                    }}
                  />
                  <div style={{ fontSize: 11, color: 'var(--mm-color-text-secondary)', marginTop: 4 }}>
                    MP4, MOV, MKV or WebM. The server converts it to the ad format and measures the duration.
                  </div>
                  {uploadError && (
                    <div style={{ color: 'var(--mm-color-error)', marginTop: 4 }}>{uploadError}</div>
                  )}
                </div>

                <div style={formGroupStyle}>
                  <label style={labelStyle}>Click-through URL (optional)</label>
                  <input
                    style={inputStyle}
                    value={createClickUrl}
                    onChange={(e) => setCreateClickUrl(e.target.value)}
                    placeholder="https://advertiser.com/landing"
                  />
                </div>

                <div style={formGroupStyle}>
                  <label style={labelStyle}>Categories (comma-separated, optional)</label>
                  <input
                    style={inputStyle}
                    value={createCategories}
                    onChange={(e) => setCreateCategories(e.target.value)}
                    placeholder="tech, gaming"
                  />
                </div>

                <div className="dialog-actions">
                  <button className="btn btn-ghost" onClick={handleCloseCreate} disabled={busy}>
                    Cancel
                  </button>
                  <button
                    className="btn btn-primary"
                    onClick={handleCreate}
                    disabled={busy || !createTitle.trim()}
                  >
                    {busy ? (uploadPct > 0 ? `Uploading… ${uploadPct}%` : 'Creating...') : 'Create'}
                  </button>
                </div>
              </>
            )}
          </div>
        </div>
      )}

      {/* ---- Edit Ad Dialog ---- */}
      {editingAd && (
        <div className="dialog-overlay" onClick={() => setEditingAd(null)}>
          <div className="dialog" style={{ maxWidth: 500 }} onClick={(e) => e.stopPropagation()}>
            <h2>Edit Ad</h2>
            <p style={{ marginBottom: 'var(--mm-space-sm)' }}>
              ID: <span className="mono">{truncateId(editingAd.id)}</span>
            </p>

            <div style={formGroupStyle}>
              <label style={labelStyle}>Title</label>
              <input
                style={inputStyle}
                value={editTitle}
                onChange={(e) => setEditTitle(e.target.value)}
                autoFocus
              />
            </div>

            <div style={formGroupStyle}>
              <label style={labelStyle}>Placement</label>
              <select
                style={selectStyle}
                value={editPlacement}
                onChange={(e) => setEditPlacement(e.target.value)}
              >
                {PLACEMENT_OPTIONS.map((p) => (
                  <option key={p} value={p}>
                    {placementLabel(p)}
                  </option>
                ))}
              </select>
            </div>

            <div style={formGroupStyle}>
              <label style={labelStyle}>Status</label>
              {editingAd.status === 'ready' || editingAd.status === 'paused' ? (
                <select
                  style={selectStyle}
                  value={editStatus}
                  onChange={(e) => setEditStatus(e.target.value)}
                >
                  <option value="ready">Ready</option>
                  <option value="paused">Paused</option>
                </select>
              ) : (
                <div>
                  <span className={statusBadgeClass(editingAd.status)}>{editingAd.status}</span>
                  <div style={{ fontSize: 11, color: 'var(--mm-color-text-secondary)', marginTop: 4 }}>
                    Set by conversion. An ad becomes ready once a file has been converted.
                  </div>
                </div>
              )}
            </div>

            <div style={formGroupStyle}>
              <label style={labelStyle}>Click-through URL</label>
              <input
                style={inputStyle}
                value={editClickUrl}
                onChange={(e) => setEditClickUrl(e.target.value)}
                placeholder="https://advertiser.com/landing"
              />
            </div>

            <div style={formGroupStyle}>
              <label style={labelStyle}>Categories (comma-separated)</label>
              <input
                style={inputStyle}
                value={editCategories}
                onChange={(e) => setEditCategories(e.target.value)}
                placeholder="tech, gaming"
              />
            </div>

            <div className="dialog-actions">
              <button className="btn btn-ghost" onClick={() => setEditingAd(null)} disabled={busy}>
                Cancel
              </button>
              <button
                className="btn btn-primary"
                onClick={handleEdit}
                disabled={busy || !editTitle.trim()}
              >
                {busy ? 'Saving...' : 'Save'}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* ---- Upload Dialog ---- */}
      {uploadTarget && (
        <div className="dialog-overlay" onClick={handleCloseUpload}>
          <div className="dialog" style={{ maxWidth: 480 }} onClick={(e) => e.stopPropagation()}>
            <h2>Upload Media</h2>
            <p>
              Upload a video file for ad{' '}
              <span className="mono">{truncateId(uploadTarget.id)}</span>.
              The server converts it to the ad format and measures the duration.
            </p>

            <div style={formGroupStyle}>
              <label style={labelStyle}>Video file</label>
              <input
                type="file"
                accept="video/*"
                style={{ ...inputStyle, padding: '8px' }}
                onChange={(e) => {
                  setUploadFile(e.target.files?.[0] ?? null);
                  setUploadError('');
                }}
              />
              <div style={{ fontSize: 11, color: 'var(--mm-color-text-secondary)', marginTop: 4 }}>
                MP4, MOV, MKV or WebM.
              </div>
            </div>
            {uploadError && (
              <div style={{ color: 'var(--mm-color-error)', marginBottom: 'var(--mm-space-md)' }}>
                {uploadError}
              </div>
            )}
            <div className="dialog-actions">
              <button className="btn btn-ghost" onClick={handleCloseUpload} disabled={uploading}>
                Cancel
              </button>
              <button
                className="btn btn-primary"
                onClick={handleUpload}
                disabled={uploading || !uploadFile}
              >
                {uploading ? `Uploading… ${uploadPct}%` : 'Upload'}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* ---- Delete Confirmation Dialog ---- */}
      {deleteTarget && (
        <div className="dialog-overlay" onClick={() => setDeleteTarget(null)}>
          <div className="dialog" onClick={(e) => e.stopPropagation()}>
            <h2>Delete Ad?</h2>
            <p>
              Are you sure you want to delete{' '}
              <strong>{deleteTarget.title}</strong>? This action cannot be undone.
            </p>
            <div className="dialog-actions">
              <button
                className="btn btn-ghost"
                onClick={() => setDeleteTarget(null)}
                disabled={busy}
              >
                Cancel
              </button>
              <button
                className="btn btn-danger"
                onClick={handleDelete}
                disabled={busy}
              >
                {busy ? 'Deleting...' : 'Delete'}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
