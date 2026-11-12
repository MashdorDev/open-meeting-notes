'use client';

import { useEffect, useState } from 'react';
import { Pencil, Check, X } from 'lucide-react';

interface OtherSpeakerNameProps {
  names: string[];
  onSave: (names: string[]) => Promise<void>;
}

/** Shows who the "Others" lines belong to, and lets Dor rename or clear it. */
export function OtherSpeakerName({ names, onSave }: OtherSpeakerNameProps) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState('');
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (!editing) setDraft(names.join(', '));
  }, [names, editing]);

  const save = async () => {
    setSaving(true);
    try {
      await onSave(draft.split(',').map(n => n.trim()).filter(Boolean));
      setEditing(false);
    } finally {
      setSaving(false);
    }
  };

  if (!editing) {
    return (
      <div className="mt-2 flex items-center gap-1 text-sm text-gray-600">
        <span>Other side:</span>
        <span className="font-semibold text-emerald-600">{names.length > 0 ? names.join(', ') : 'Others'}</span>
        <button
          type="button"
          onClick={() => setEditing(true)}
          className="ml-1 rounded p-1 text-gray-400 hover:bg-gray-100 hover:text-gray-700"
          aria-label="Edit the other side's name"
          title="Edit name"
        >
          <Pencil className="h-3.5 w-3.5" />
        </button>
      </div>
    );
  }

  return (
    <form
      className="mt-2 flex items-center gap-1 text-sm"
      onSubmit={e => { e.preventDefault(); void save(); }}
    >
      <label htmlFor="other-speaker-name" className="text-gray-600">Other side:</label>
      <input
        id="other-speaker-name"
        autoFocus
        value={draft}
        onChange={e => setDraft(e.target.value)}
        onKeyDown={e => { if (e.key === 'Escape') setEditing(false); }}
        placeholder="Name, or several separated by commas"
        className="min-w-0 flex-1 rounded border border-gray-300 px-2 py-0.5 focus:border-emerald-500 focus:outline-none"
        disabled={saving}
      />
      <button type="submit" disabled={saving} className="rounded p-1 text-emerald-600 hover:bg-emerald-50" aria-label="Save name" title="Save">
        <Check className="h-4 w-4" />
      </button>
      <button type="button" onClick={() => setEditing(false)} disabled={saving} className="rounded p-1 text-gray-400 hover:bg-gray-100" aria-label="Cancel" title="Cancel">
        <X className="h-4 w-4" />
      </button>
    </form>
  );
}
