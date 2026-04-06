import { useState, useCallback, type KeyboardEvent } from 'react';
import type { ServerConfig } from '../types';
import { updateConfig } from '../api/AdminApiClient';

interface ConfigFormProps {
  config: ServerConfig;
  onUpdated: (config: ServerConfig) => void;
}

export function ConfigForm({ config, onUpdated }: ConfigFormProps) {
  const [editingKey, setEditingKey] = useState<string | null>(null);
  const [editValue, setEditValue] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');

  const entries = Object.entries(config).sort(([a], [b]) => a.localeCompare(b));

  const startEditing = useCallback((key: string, value: unknown) => {
    setEditingKey(key);
    setEditValue(typeof value === 'string' ? value : JSON.stringify(value));
    setError('');
  }, []);

  const cancelEditing = useCallback(() => {
    setEditingKey(null);
    setEditValue('');
    setError('');
  }, []);

  const saveValue = useCallback(
    async (key: string) => {
      setSaving(true);
      setError('');
      try {
        // Try to parse as JSON first; fall back to string
        let parsed: unknown;
        try {
          parsed = JSON.parse(editValue);
        } catch {
          parsed = editValue;
        }

        const updated = await updateConfig({ [key]: parsed });
        onUpdated(updated);
        setEditingKey(null);
        setEditValue('');
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Save failed');
      } finally {
        setSaving(false);
      }
    },
    [editValue, onUpdated],
  );

  const handleKeyDown = useCallback(
    (e: KeyboardEvent, key: string) => {
      if (e.key === 'Enter') {
        void saveValue(key);
      } else if (e.key === 'Escape') {
        cancelEditing();
      }
    },
    [saveValue, cancelEditing],
  );

  if (entries.length === 0) {
    return (
      <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
        <p style={{ color: 'var(--mm-color-text-secondary)' }}>
          No configuration keys
        </p>
      </div>
    );
  }

  return (
    <div className="card">
      {error && (
        <div style={{ color: 'var(--mm-color-error)', marginBottom: 'var(--mm-space-md)', fontSize: '0.8125rem' }}>
          {error}
        </div>
      )}
      <div className="table-container" style={{ border: 'none' }}>
        <table>
          <thead>
            <tr>
              <th>Key</th>
              <th>Value</th>
              <th style={{ width: 80 }}>Action</th>
            </tr>
          </thead>
          <tbody>
            {entries.map(([key, value]) => (
              <tr key={key}>
                <td>
                  <span className="config-key">{key}</span>
                </td>
                <td>
                  {editingKey === key ? (
                    <input
                      className="input"
                      value={editValue}
                      onChange={(e) => setEditValue(e.target.value)}
                      onKeyDown={(e) => handleKeyDown(e, key)}
                      autoFocus
                      disabled={saving}
                      style={{ maxWidth: 300 }}
                    />
                  ) : (
                    <span
                      className="mono"
                      style={{ cursor: 'pointer' }}
                      onClick={() => startEditing(key, value)}
                      title="Click to edit"
                    >
                      {typeof value === 'string'
                        ? value
                        : JSON.stringify(value)}
                    </span>
                  )}
                </td>
                <td>
                  {editingKey === key ? (
                    <div style={{ display: 'flex', gap: 'var(--mm-space-xs)' }}>
                      <button
                        className="btn btn-primary btn-sm"
                        onClick={() => saveValue(key)}
                        disabled={saving}
                      >
                        {saving ? '...' : 'Save'}
                      </button>
                      <button
                        className="btn btn-ghost btn-sm"
                        onClick={cancelEditing}
                        disabled={saving}
                      >
                        Cancel
                      </button>
                    </div>
                  ) : (
                    <button
                      className="btn btn-ghost btn-sm"
                      onClick={() => startEditing(key, value)}
                    >
                      Edit
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
