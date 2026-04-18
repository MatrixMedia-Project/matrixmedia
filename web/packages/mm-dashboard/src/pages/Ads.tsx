import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import type {
  AdCreativeInfo,
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
  getAdStats,
  getAdAnalytics,
} from '../api/AdminApiClient';
import { isAdmin } from '../auth/AdminAuth';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type StatusFilter = 'all' | 'ready' | 'paused' | 'processing';

const STATUS_OPTIONS: StatusFilter[] = ['all', 'ready', 'paused', 'processing'];

const PLACEMENT_OPTIONS = ['pre_roll', 'mid_roll', 'post_roll', 'any'] as const;

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

function truncateId(id: string, max = 12): string {
  if (id.length <= max) return id;
  return `${id.slice(0, max - 4)}...`;
}

function formatDuration(secs: number): string {
  if (secs <= 0) return '--';
  return `${secs}s`;
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

function statusBadgeClass(status: string): string {
  switch (status) {
    case 'ready':
      return 'badge badge-active';
    case 'paused':
    case 'processing':
      return 'badge badge-ended';
    case 'deleted':
      return 'badge badge-ended';
    default:
      return 'badge';
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
  const [createMediaUrl, setCreateMediaUrl] = useState('');
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
  const [uploadResult, setUploadResult] = useState<{ cdn_url: string; duration_secs: number } | null>(null);

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
    const interval = setInterval(() => {
      if (visibleRef.current) void fetchData();
    }, 15_000);
    return () => clearInterval(interval);
  }, [fetchData]);

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
    setCreateMediaUrl('');
    setCreateClickUrl('');
    setCreateCategories('');
    setCreateFile(null);
    setLastCreatedId(null);
  }, []);

  const handleOpenCreate = useCallback(() => {
    resetCreateForm();
    setShowCreate(true);
  }, [resetCreateForm]);

  const handleCreate = useCallback(async () => {
    if (!createTitle.trim()) return;
    setBusy(true);
    try {
      const req: CreateAdRequest = {
        title: createTitle.trim(),
        placement: createPlacement,
      };
      if (createMediaUrl.trim()) req.cdn_url = createMediaUrl.trim();
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
        setMessage(`Ad created. Uploading video file...`);
        try {
          const uploadResult = await uploadAdFile(result.id, createFile);
          setMessage(`Ad created with video (${uploadResult.duration_secs}s). ID: ${truncateId(result.id)}`);
        } catch (uploadErr) {
          setMessage(`Ad created but upload failed: ${uploadErr instanceof Error ? uploadErr.message : 'unknown error'}. You can upload later.`);
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
  }, [createTitle, createPlacement, createMediaUrl, createClickUrl, createCategories, createFile, fetchData]);

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
      await updateAd(editingAd.id, updates);
      setEditingAd(null);
      setMessage(`Ad updated: ${truncateId(editingAd.id)}`);
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
    setUploadResult(null);
  }, []);

  const handleUpload = useCallback(async () => {
    if (!uploadTarget || !uploadFile) return;
    setUploading(true);
    try {
      const result = await uploadAdFile(uploadTarget.id, uploadFile);
      setUploadResult({ cdn_url: result.cdn_url, duration_secs: result.duration_secs });
      setMessage(`File uploaded for ad ${truncateId(uploadTarget.id)}`);
      await fetchData();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to upload file');
    } finally {
      setUploading(false);
    }
  }, [uploadTarget, uploadFile, fetchData]);

  const handleCloseUpload = useCallback(() => {
    setUploadTarget(null);
    setUploadFile(null);
    setUploadResult(null);
  }, []);

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
          <td>{formatDuration(ad.duration_secs)}</td>
          <td>
            <span className={statusBadgeClass(ad.status)}>{ad.status}</span>
          </td>
          <td>
            {ad.cdn_url ? (
              <a
                href={ad.cdn_url}
                target="_blank"
                rel="noopener noreferrer"
                style={{ color: 'var(--mm-color-primary)' }}
              >
                View
              </a>
            ) : (
              <span style={{ color: 'var(--mm-color-text-secondary)' }}>No file</span>
            )}
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
  }, [filteredAds, statsAdId, adStats, statsLoading, handleToggleStats, handleOpenEdit, handleOpenUpload]);

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
          <p>Manage advertising creatives. Auto-refreshes every 15s.</p>
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
                    onChange={(e) => setCreateFile(e.target.files?.[0] ?? null)}
                  />
                  <div style={{ fontSize: 11, color: 'var(--mm-color-text-secondary)', marginTop: 4 }}>
                    MP4 or WebM. Server transcodes to WebM and auto-detects duration.
                  </div>
                </div>

                <div style={formGroupStyle}>
                  <label style={labelStyle}>Or paste a media URL</label>
                  <input
                    style={inputStyle}
                    value={createMediaUrl}
                    onChange={(e) => setCreateMediaUrl(e.target.value)}
                    placeholder="https://cdn.example.com/ad.mp4"
                  />
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
                    {busy ? 'Creating...' : 'Create'}
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
              <select
                style={selectStyle}
                value={editStatus}
                onChange={(e) => setEditStatus(e.target.value)}
              >
                <option value="ready">Ready</option>
                <option value="paused">Paused</option>
              </select>
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
              The server will detect the duration automatically.
            </p>

            {uploadResult ? (
              <>
                <div
                  className="card"
                  style={{
                    marginBottom: 'var(--mm-space-md)',
                    padding: 'var(--mm-space-md)',
                  }}
                >
                  <div style={{ marginBottom: 4 }}>
                    <strong>CDN URL:</strong>{' '}
                    <a
                      href={uploadResult.cdn_url}
                      target="_blank"
                      rel="noopener noreferrer"
                      style={{ color: 'var(--mm-color-primary)', wordBreak: 'break-all' }}
                    >
                      {uploadResult.cdn_url}
                    </a>
                  </div>
                  <div>
                    <strong>Duration:</strong> {formatDuration(uploadResult.duration_secs)}
                  </div>
                </div>
                <div className="dialog-actions">
                  <button className="btn btn-primary" onClick={handleCloseUpload}>
                    Done
                  </button>
                </div>
              </>
            ) : (
              <>
                <div style={formGroupStyle}>
                  <label style={labelStyle}>Video file</label>
                  <input
                    type="file"
                    accept="video/*"
                    style={{ ...inputStyle, padding: '8px' }}
                    onChange={(e) => setUploadFile(e.target.files?.[0] ?? null)}
                  />
                </div>
                <div className="dialog-actions">
                  <button className="btn btn-ghost" onClick={handleCloseUpload} disabled={uploading}>
                    Cancel
                  </button>
                  <button
                    className="btn btn-primary"
                    onClick={handleUpload}
                    disabled={uploading || !uploadFile}
                  >
                    {uploading ? 'Uploading...' : 'Upload'}
                  </button>
                </div>
              </>
            )}
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
