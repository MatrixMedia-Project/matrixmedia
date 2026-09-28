interface Props {
  count: number;
  saving: boolean;
  disabled: boolean;
  /** Why Save is disabled, when the reason may be out of view (e.g. on another tab). */
  note?: string;
  onSave: () => void;
  onDiscard: () => void;
}

export function SaveBar({ count, saving, disabled, note, onSave, onDiscard }: Props) {
  return (
    <div className="settings-savebar" role="region" aria-label="Unsaved changes">
      <span>{count} unsaved change{count === 1 ? '' : 's'}</span>
      {note && <span className="settings-savebar-note">{note}</span>}
      <button type="button" className="btn btn-ghost" onClick={onDiscard} disabled={saving}>
        Discard
      </button>
      <button type="button" className="btn btn-primary" onClick={onSave} disabled={saving || disabled}>
        {saving ? 'Saving…' : 'Save'}
      </button>
    </div>
  );
}
