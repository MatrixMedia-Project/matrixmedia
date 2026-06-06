import { describe, it, expect, vi, beforeEach } from 'vitest';
import type { ModerationReport } from '../types';

// Mock the admin API module the page imports. The mock factory must be
// self-contained (hoisted by Vitest), so the fixture is declared inside it
// and re-exposed via the mocked functions.
vi.mock('../api/AdminApiClient', () => {
  const report: ModerationReport = {
    id: 'rep_1',
    source: 'mm',
    target_type: 'stream',
    target_id: 'stream_abc',
    room_id: '!room:matrix.steegler.com',
    reported_user_id: null,
    reporter_id: '@reporter:matrix.steegler.com',
    reason: 'abuse',
    details: null,
    status: 'open',
    created_at: '2026-06-06T12:00:00Z',
    resolved_by: null,
    resolved_at: null,
  };
  return {
    listReports: vi.fn(async () => [report]),
    getReport: vi.fn(async () => ({ report, actions: [] })),
    syncReports: vi.fn(async () => ({ ok: true, ingested: 0 })),
    setReportStatus: vi.fn(async () => ({ ok: true })),
    applyAction: vi.fn(async () => ({ ok: true })),
    listAudit: vi.fn(async () => []),
    AdminApiError: class extends Error {},
  };
});

import * as api from '../api/AdminApiClient';
import { buildQueueRow, actionsForReport, type ModerationActionDef } from './Moderation';

type ActionDef = Extract<ModerationActionDef, { kind: 'action' }>;
const isAction = (a: ModerationActionDef): a is ActionDef => a.kind === 'action';

const openMmStreamReport: ModerationReport = {
  id: 'rep_1',
  source: 'mm',
  target_type: 'stream',
  target_id: 'stream_abc',
  room_id: '!room:matrix.steegler.com',
  reported_user_id: null,
  reporter_id: '@reporter:matrix.steegler.com',
  reason: 'abuse',
  details: null,
  status: 'open',
  created_at: '2026-06-06T12:00:00Z',
  resolved_by: null,
  resolved_at: null,
};

describe('Moderation queue', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('the mocked api resolves one open mm/stream report', async () => {
    const reports = await api.listReports('open');
    expect(reports).toHaveLength(1);
    expect(reports[0]?.source).toBe('mm');
    expect(reports[0]?.target_type).toBe('stream');
    expect(reports[0]?.status).toBe('open');
  });

  it('buildQueueRow renders the report reason and target type', () => {
    const row = buildQueueRow(openMmStreamReport);
    expect(row.reason).toBe('abuse');
    expect(row.targetType).toBe('stream');
    expect(row.source).toBe('mm');
    // subject falls back to target_id when there is no reported user
    expect(row.subject).toBe('stream_abc');
    expect(row.status).toBe('open');
  });
});

describe('actionsForReport mapping', () => {
  it('a stream report offers Force-stop (force_stop_stream / stream target)', () => {
    const actions = actionsForReport(openMmStreamReport);
    const forceStop = actions.find(
      (a: ModerationActionDef) => a.kind === 'action' && a.actionType === 'force_stop_stream',
    );
    expect(forceStop).toBeDefined();
    expect(forceStop?.kind).toBe('action');
    if (forceStop?.kind === 'action') {
      expect(forceStop.targetType).toBe('stream');
      expect(forceStop.targetId).toBe('stream_abc');
    }
    // Dismiss is always available.
    expect(actions.some((a) => a.kind === 'dismiss')).toBe(true);
  });

  it('a recording report offers Hide / Unhide / Delete', () => {
    const rec: ModerationReport = {
      ...openMmStreamReport,
      target_type: 'recording',
      target_id: 'rec_1',
    };
    const types = actionsForReport(rec)
      .filter(isAction)
      .map((a) => a.actionType);
    expect(types).toContain('hide_recording');
    expect(types).toContain('unhide_recording');
    expect(types).toContain('delete_recording');
  });

  it('a user report (or reported_user_id present) offers Suspend / Unsuspend / Deactivate targeting the user', () => {
    const userRep: ModerationReport = {
      ...openMmStreamReport,
      target_type: 'user',
      target_id: '@bad:matrix.steegler.com',
      reported_user_id: '@bad:matrix.steegler.com',
    };
    const actions = actionsForReport(userRep).filter(isAction);
    const types = actions.map((a) => a.actionType);
    expect(types).toContain('suspend_user');
    expect(types).toContain('unsuspend_user');
    expect(types).toContain('deactivate_user');
    // Targets the reported user id.
    expect(actions.every((a) => a.targetType === 'user')).toBe(true);
    expect(actions.every((a) => a.targetId === '@bad:matrix.steegler.com')).toBe(true);
  });

  it('an event report with a reported_user_id still offers user actions', () => {
    const eventRep: ModerationReport = {
      ...openMmStreamReport,
      target_type: 'event',
      target_id: '$evt:matrix.steegler.com',
      reported_user_id: '@offender:matrix.steegler.com',
    };
    const types = actionsForReport(eventRep)
      .filter(isAction)
      .map((a) => a.actionType);
    expect(types).toContain('suspend_user');
  });
});
