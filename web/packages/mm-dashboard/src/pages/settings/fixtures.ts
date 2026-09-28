import type { SettingSchema, SettingsState, SettingValueView } from '../../types';

export function schema(over: Partial<SettingSchema> & { key: string }): SettingSchema {
  return {
    group: 'general',
    kind: { type: 'text' },
    class: { kind: 'live' },
    secret: false,
    env: null,
    description: `About ${over.key}`,
    ...over,
  };
}

export function view(over: Partial<SettingValueView> = {}): SettingValueView {
  return {
    source: 'database',
    env_shadowed: false,
    pending: false,
    updated_at: null,
    updated_by: null,
    ...over,
  };
}

export function makeState(
  entries: [SettingSchema, SettingValueView][],
  over: Partial<SettingsState> = {},
): SettingsState {
  return {
    schema: entries.map(([s]) => s),
    values: Object.fromEntries(entries.map(([s, v]) => [s.key, v])),
    safe_mode: false,
    safe_mode_reason: null,
    loaded_rev: 10,
    current_rev: 10,
    pending_restart: [],
    encryption_key_configured: true,
    rows_on_previous_key: 0,
    secret_problems: [],
    live_reload_error: null,
    demo: false,
    ...over,
  };
}
