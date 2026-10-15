import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import type {
  AdCreativeInfo,
  AdStatus,
  AdWorkerStatus,
  CreateAdRequest,
  UpdateAdRequest,
  AdStatsResponse,
  AdAnalyticsResponse,
} from '../types';
import {
  listAdsWithWorker,
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
  isLegacyFile,
  NO_WORKER_TEXT,
  oversizeText,
  progressText,
  uploadedText,
  uploadErrorText,
  warningText,
  workerMissing,
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

/** How often the list refreshes: fast while a conversion runs (backing off on failures), else slow. */
const FAST_POLL_MS = 2_000;
const SLOW_POLL_MS = 15_000;

function ProgressBar({ pct, label }: { pct: number; label: string }) {
  const clamped = Math.max(0, Math.min(100, pct));
  return (
    <div
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={clamped}
      style={{ height: 4, borderRadius: 2, marginTop: 2, overflow: 'hidden', background: 'var(--mm-color-surface-elevated)' }}
    >
      <div style={{ width: `${clamped}%`, height: '100%', background: 'var(--mm-color-primary)' }} />
    </div>
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
  const busy = isBusy(job);
  const failure = job?.error ?? null;
  // The live media's warnings are the creative's own. The newest job's are that attempt's: while a
  // newer upload converts or fails they describe the new upload, not what is playing.
  const liveWarnings = ad.media?.media_warnings ?? [];
  const newUploadWarnings = job && (busy || job.state === 'failed') ? job.warnings : [];
  return (
    <>
      {ad.preview_url && (
        <div>
          <a
            href={ad.preview_url}
            target="_blank"
            rel="noopener noreferrer"
            aria-label={`View ${ad.title}`}
            style={{ color: 'var(--mm-color-primary)' }}
          >
            View
          </a>
          {ad.media?.vp8 && <div style={secondaryLine}>{factLine(ad.media.vp8)}</div>}
        </div>
      )}
      {liveWarnings.map((w, i) => (
        <div key={`live-${i}-${w}`} style={secondaryLine}>{warningText(w)}</div>
      ))}
      {job && busy && (
        <div>
          <div style={{ color: 'var(--mm-color-text-secondary)' }}>{progressText(job)}</div>
          <ProgressBar pct={job.progress_pct} label={`Conversion progress for ${ad.title}`} />
        </div>
      )}
      {failure && (
        <div>
          <div>{failure.message}</div>
          <div style={secondaryLine}>{failure.hint}</div>
        </div>
      )}
      {newUploadWarnings.map((w, i) => (
        <div key={`new-${i}-${w}`} style={secondaryLine}>New upload: {warningText(w)}</div>
      ))}
      {!ad.preview_url && !busy && !failure && (
        <span style={{ color: 'var(--mm-color-text-secondary)' }}>
          {isLegacyFile(ad) ? 'Legacy file (re-encoding pending)' : 'No file'}
        </span>
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
  // The conversion worker's status from the latest list; null until one arrives.
  const [worker, setWorker] = useState<AdWorkerStatus | null>(null);
  const [loading, setLoading] = useState(true);
  // Two kinds of error. A failed fetch or poll is `loadError`: the next good fetch clears it.
  // A failed action (delete, retry, stats) is `actionError`: polling never clears it, only the
  // next user action or the Dismiss button does.
  const [loadError, setLoadError] = useState('');
  const [actionError, setActionError] = useState('');
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
  const [createError, setCreateError] = useState('');

  // Edit form
  const [editTitle, setEditTitle] = useState('');
  const [editPlacement, setEditPlacement] = useState<string>('pre_roll');
  const [editClickUrl, setEditClickUrl] = useState('');
  const [editCategories, setEditCategories] = useState('');
  const [editStatus, setEditStatus] = useState<string>('ready');
  const [editError, setEditError] = useState('');

  // Upload
  const [uploadFile, setUploadFile] = useState<File | null>(null);
  const [uploading, setUploading] = useState(false);
  const [uploadPct, setUploadPct] = useState(0);
  // Dialog errors are shown inside the dialog: a page-level one sits behind its overlay.
  const [uploadError, setUploadError] = useState('');

  // Last created ad ID (for post-create UX)
  const [lastCreatedId, setLastCreatedId] = useState<string | null>(null);

  // -----------------------------------------------------------------------
  // Data fetching
  // -----------------------------------------------------------------------

  // Every fetch takes a sequence number when it starts. An answer older than one already on
  // screen is dropped, so a poll that began before a retry cannot overwrite the retry's fresh list.
  const sequenceRef = useRef(0);
  const appliedRef = useRef(0);
  const inFlightRef = useRef(0);
  const failuresRef = useRef(0);
  const analyticsAtRef = useRef(0);

  // The analytics cards change slowly, so only a slow tick or an explicit refresh fetches them.
  const fetchData = useCallback(async (withAnalytics = true) => {
    const seq = ++sequenceRef.current;
    inFlightRef.current += 1;
    try {
      const [listed, analyticsData] = await Promise.all([
        listAdsWithWorker(),
        withAnalytics ? getAdAnalytics() : Promise.resolve(null),
      ]);
      if (seq < appliedRef.current) return;
      appliedRef.current = seq;
      failuresRef.current = 0;
      setAds(listed.ads);
      setWorker(listed.worker);
      if (analyticsData) {
        setAnalytics(analyticsData);
        analyticsAtRef.current = Date.now();
      }
      setLoadError('');
    } catch (err) {
      if (seq < appliedRef.current) return;
      failuresRef.current += 1;
      setLoadError(err instanceof Error ? err.message : 'Failed to fetch ads');
    } finally {
      inFlightRef.current -= 1;
      setLoading(false);
    }
  }, []);

  // Which dialog is open, for outcomes that arrive after the user has moved on: an error goes
  // inside its dialog while that dialog is still open, and to the page otherwise.
  const openDialogsRef = useRef<{ create: boolean; editId: string | null; uploadId: string | null }>({
    create: false,
    editId: null,
    uploadId: null,
  });
  useEffect(() => {
    openDialogsRef.current = { create: showCreate, editId: editingAd?.id ?? null, uploadId: uploadTarget?.id ?? null };
  }, [showCreate, editingAd, uploadTarget]);

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

  // Poll faster while a conversion is running or queued, backing off 2, 4, 8, 15 s while the
  // fetches keep failing and starting again at 2 s after a good one; 15 s when nothing is
  // converting. Depend on the boolean, not on `ads`: a new list arrives on every poll, and
  // re-running the effect on it would refetch at once. A tick never starts while another fetch
  // is in flight, and the next one is scheduled only after this one has finished.
  const converting = anyBusy(ads);
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let stopped = false;
    const delay = () => (converting ? Math.min(FAST_POLL_MS * 2 ** failuresRef.current, SLOW_POLL_MS) : SLOW_POLL_MS);
    const tick = async () => {
      if (visibleRef.current && inFlightRef.current === 0) {
        await fetchData(Date.now() - analyticsAtRef.current >= SLOW_POLL_MS - 500);
      }
      if (!stopped) timer = setTimeout(tick, delay());
    };
    timer = setTimeout(tick, delay());
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
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
      setActionError('');
      setStatsAdId(adId);
      setStatsLoading(true);
      try {
        const s = await getAdStats(adId);
        setAdStats(s);
      } catch (err) {
        setActionError(err instanceof Error ? err.message : 'Failed to fetch ad stats');
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
    setCreateError('');
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
    setCreateError('');
    setActionError('');
    try {
      // Refuse an over-size file before creating anything: a refused upload would leave a draft.
      if (createFile) {
        const tooBig = oversizeText(createFile, await getAdUploadLimitMb());
        if (tooBig) {
          setCreateError(tooBig);
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
          const accepted = await uploadAdFile(result.id, createFile, (f) => {
            setUploadPct(Math.round(f * 100));
            setMessage(`Ad created. Uploading video file… ${Math.round(f * 100)}%`);
          });
          setMessage(uploadedText(accepted));
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
      const text = err instanceof Error ? err.message : 'Failed to create ad';
      if (openDialogsRef.current.create) setCreateError(text);
      else setActionError(text);
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
    setEditError('');
  }, []);

  const handleEdit = useCallback(async () => {
    if (!editingAd) return;
    setBusy(true);
    setEditError('');
    setActionError('');
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
      // Inside the dialog, which is still open: the 400s (slot cap, bad placement) and the 409
      // "This ad was deleted." are what the admin has to read before trying again.
      const text = err instanceof Error ? err.message : 'Failed to update ad';
      if (openDialogsRef.current.editId === editingAd.id) setEditError(text);
      else setActionError(text);
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
    const targetId = uploadTarget.id;
    // The dialog can be closed or pointed at another ad while the file goes up (the overlay does
    // not close it, but keyboard focus is not trapped). The outcome is then reported on the page.
    const stillOpen = () => openDialogsRef.current.uploadId === targetId;
    setUploading(true);
    setUploadPct(0);
    setUploadError('');
    setActionError('');
    try {
      const tooBig = oversizeText(uploadFile, await getAdUploadLimitMb());
      if (tooBig) {
        setUploadError(tooBig);
        return;
      }
      // Deliberately not aborted when the user navigates away: the upload keeps going, and its
      // outcome is reported only while this page is mounted.
      const accepted = await uploadAdFile(targetId, uploadFile, (f) => {
        if (stillOpen()) setUploadPct(Math.round(f * 100));
      });
      if (stillOpen()) {
        setUploadTarget(null);
        setUploadFile(null);
      }
      setMessage(uploadedText(accepted));
      await fetchData();
    } catch (err) {
      const text = uploadErrorText(err);
      if (stillOpen()) setUploadError(text);
      else setActionError(`Upload for ad ${truncateId(targetId)} failed: ${text}`);
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
      setActionError('');
      try {
        await retryAdConversion(adId);
        setMessage('Retrying the conversion.');
        await fetchData();
      } catch (err) {
        setActionError(err instanceof Error ? err.message : 'Retry failed');
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
    setActionError('');
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
      // Close the confirmation so the error, which stays until dismissed, is not behind it.
      setDeleteTarget(null);
      setActionError(err instanceof Error ? err.message : 'Failed to delete ad');
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
              {ad.media?.job?.error?.retryable && (
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

      {/* Conversions wait for a worker; say so when none is running */}
      {converting && workerMissing(worker) && (
        <div className="banner banner-warning" role="status">{NO_WORKER_TEXT}</div>
      )}

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

      {/* Fetch / poll failure: the next good fetch clears it */}
      {loadError && (
        <div
          className="card"
          role="alert"
          style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}
        >
          {loadError}
        </div>
      )}

      {/* Failed action: stays until the next action or Dismiss */}
      {actionError && (
        <div
          className="card"
          role="alert"
          style={{
            marginBottom: 'var(--mm-space-lg)',
            color: 'var(--mm-color-error)',
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'center',
            gap: 'var(--mm-space-md)',
          }}
        >
          <span>{actionError}</span>
          <button className="btn btn-sm btn-ghost" onClick={() => setActionError('')}>
            Dismiss
          </button>
        </div>
      )}

      {/* Success message */}
      {message && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-lg)' }}>
          {message}
        </div>
      )}

      {/* Ad listing table */}
      {loading && !loadError && ads.length === 0 ? (
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
        <div className="dialog-overlay" onClick={busy ? undefined : handleCloseCreate}>
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
                      setCreateError('');
                    }}
                  />
                  <div style={{ fontSize: 11, color: 'var(--mm-color-text-secondary)', marginTop: 4 }}>
                    MP4, MOV, MKV or WebM. The server converts it to the ad format and measures the duration.
                  </div>
                  {createError && (
                    <div role="alert" style={{ color: 'var(--mm-color-error)', marginTop: 4 }}>{createError}</div>
                  )}
                  {busy && uploadPct > 0 && <ProgressBar pct={uploadPct} label="Upload progress" />}
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
        <div className="dialog-overlay" onClick={busy ? undefined : () => setEditingAd(null)}>
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

            {editError && (
              <div role="alert" style={{ color: 'var(--mm-color-error)', marginBottom: 'var(--mm-space-md)' }}>
                {editError}
              </div>
            )}
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
        <div className="dialog-overlay" onClick={uploading ? undefined : handleCloseUpload}>
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
            {uploading && <ProgressBar pct={uploadPct} label="Upload progress" />}
            {uploadError && (
              <div role="alert" style={{ color: 'var(--mm-color-error)', marginBottom: 'var(--mm-space-md)' }}>
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
        <div className="dialog-overlay" onClick={busy ? undefined : () => setDeleteTarget(null)}>
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
