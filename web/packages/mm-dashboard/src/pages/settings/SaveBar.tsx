interface Props {
  count: number;
  saving: boolean;
  disabled: boolean;
  /** Why Save is disabled, when the reason may be out of view (e.g. on another tab). */
  note?: string;
  /** The destinations a save also sends, to confirm where its secrets go. */
  confirms?: string;
  onSave: () => void;
  onDiscard: () => void;
}

export function SaveBar({ count, saving, disabled, note, confirms, onSave, onDiscard }: Props) {
  return (
    <div className="settings-savebar" role="region" aria-label="Unsaved changes">
      <span>{count} unsaved change{count === 1 ? '' : 's'}</span>
      {note && <span className="settings-savebar-note">{note}</span>}
      {confirms && (
        <span
          className="settings-savebar-note"
          title="The server does not run this saved value yet; the save names it so the secrets go only where you confirmed"
        >
          {confirms}
        </span>
      )}
      <button type="button" className="btn btn-ghost" onClick={onDiscard} disabled={saving}>
        Discard
      </button>
      <button type="button" className="btn btn-primary" onClick={onSave} disabled={saving || disabled}>
        {saving ? 'Saving…' : 'Save'}
      </button>
    </div>
  );
}
