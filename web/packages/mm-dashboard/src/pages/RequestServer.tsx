import { useState, type FormEvent } from 'react';
import { createServerRequest, type CreateServerRequestBody } from '../api/AdminApiClient';
import { PageHeader } from '../components/PageHeader';

const REGIONS = [
  { value: 'eu-frankfurt', label: 'EU (Frankfurt)' },
  { value: 'us-east', label: 'US East' },
  { value: 'us-west', label: 'US West' },
  { value: 'asia-singapore', label: 'Asia (Singapore)' },
] as const;

const INSTANCE_SIZES = [
  { value: 'small', label: 'Small — up to ~100 users' },
  { value: 'medium', label: 'Medium — up to ~1,000 users' },
  { value: 'large', label: 'Large — up to ~10,000 users' },
  { value: 'dedicated', label: 'Dedicated / custom' },
] as const;

interface FormState {
  org_name: string;
  contact_email: string;
  region: string;
  instance_size: string;
  domain: string;
  notes: string;
}

const INITIAL: FormState = {
  org_name: '',
  contact_email: '',
  region: 'eu-frankfurt',
  instance_size: 'small',
  domain: '',
  notes: '',
};

export function RequestServer() {
  const [form, setForm] = useState<FormState>(INITIAL);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [successEmail, setSuccessEmail] = useState('');

  function setField<K extends keyof FormState>(key: K, value: FormState[K]) {
    setForm((prev) => ({ ...prev, [key]: value }));
  }

  async function onSubmit(e: FormEvent) {
    e.preventDefault();
    setSaving(true);
    setError('');
    setSuccessEmail('');

    const body: CreateServerRequestBody = {
      org_name: form.org_name.trim(),
      contact_email: form.contact_email.trim(),
      region: form.region,
      instance_size: form.instance_size,
    };
    if (form.domain.trim()) body.domain = form.domain.trim();
    if (form.notes.trim()) body.notes = form.notes.trim();

    try {
      await createServerRequest(body);
      setSuccessEmail(body.contact_email);
      setForm(INITIAL);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Request failed — please try again.');
    } finally {
      setSaving(false);
    }
  }

  return (
    <div>
      <PageHeader
        title="Request a Server"
        description="Ask us to provision a dedicated MatrixMedia instance for your organisation."
      />

      <div
        className="card"
        style={{
          maxWidth: 600,
          marginBottom: 'var(--mm-space-lg)',
          padding: 'var(--mm-space-md)',
          borderLeft: '4px solid var(--mm-color-primary)',
        }}
      >
        <p style={{ fontSize: '0.875rem', color: 'var(--mm-color-text-secondary)', margin: 0 }}>
          MatrixMedia is open-source software. This form requests a{' '}
          <strong style={{ color: 'var(--mm-color-text)' }}>hosted, managed instance</strong>{' '}
          run by the MatrixMedia team on your behalf — preconfigured with a Matrix
          homeserver, media server, and CDN. You keep full admin access. Fill in
          the form and we'll reach out within one business day to discuss pricing
          and timelines.
        </p>
      </div>

      {successEmail && (
        <div
          className="card"
          style={{
            maxWidth: 600,
            marginBottom: 'var(--mm-space-lg)',
            borderLeft: '4px solid var(--mm-color-success)',
            color: 'var(--mm-color-success)',
          }}
        >
          Request received — we'll be in touch at <strong>{successEmail}</strong>.
          Keep an eye on your inbox (and spam folder).
        </div>
      )}

      {error && (
        <div
          className="card"
          style={{
            maxWidth: 600,
            marginBottom: 'var(--mm-space-lg)',
            color: 'var(--mm-color-error)',
          }}
        >
          {error}
        </div>
      )}

      <form
        className="card"
        onSubmit={(e) => void onSubmit(e)}
        style={{ display: 'grid', gap: 'var(--mm-space-md)', maxWidth: 600 }}
      >
        {/* Organisation name */}
        <div>
          <label htmlFor="sr-org" style={{ display: 'block', marginBottom: '0.25rem', fontSize: '0.875rem', fontWeight: 600 }}>
            Organisation name <span style={{ color: 'var(--mm-color-error)' }}>*</span>
          </label>
          <input
            id="sr-org"
            className="input"
            type="text"
            required
            placeholder="Acme Broadcasting"
            value={form.org_name}
            onChange={(e) => setField('org_name', e.target.value)}
            disabled={saving}
          />
        </div>

        {/* Contact email */}
        <div>
          <label htmlFor="sr-email" style={{ display: 'block', marginBottom: '0.25rem', fontSize: '0.875rem', fontWeight: 600 }}>
            Contact email <span style={{ color: 'var(--mm-color-error)' }}>*</span>
          </label>
          <input
            id="sr-email"
            className="input"
            type="email"
            required
            placeholder="you@example.com"
            value={form.contact_email}
            onChange={(e) => setField('contact_email', e.target.value)}
            disabled={saving}
          />
        </div>

        {/* Region */}
        <div>
          <label htmlFor="sr-region" style={{ display: 'block', marginBottom: '0.25rem', fontSize: '0.875rem', fontWeight: 600 }}>
            Preferred region <span style={{ color: 'var(--mm-color-error)' }}>*</span>
          </label>
          <select
            id="sr-region"
            className="input"
            required
            value={form.region}
            onChange={(e) => setField('region', e.target.value)}
            disabled={saving}
            style={{ cursor: 'pointer' }}
          >
            {REGIONS.map((r) => (
              <option key={r.value} value={r.value}>{r.label}</option>
            ))}
          </select>
        </div>

        {/* Instance size */}
        <div>
          <label htmlFor="sr-size" style={{ display: 'block', marginBottom: '0.25rem', fontSize: '0.875rem', fontWeight: 600 }}>
            Instance size <span style={{ color: 'var(--mm-color-error)' }}>*</span>
          </label>
          <select
            id="sr-size"
            className="input"
            required
            value={form.instance_size}
            onChange={(e) => setField('instance_size', e.target.value)}
            disabled={saving}
            style={{ cursor: 'pointer' }}
          >
            {INSTANCE_SIZES.map((s) => (
              <option key={s.value} value={s.value}>{s.label}</option>
            ))}
          </select>
        </div>

        {/* Domain (optional) */}
        <div>
          <label htmlFor="sr-domain" style={{ display: 'block', marginBottom: '0.25rem', fontSize: '0.875rem', fontWeight: 600 }}>
            Domain{' '}
            <span style={{ color: 'var(--mm-color-text-secondary)', fontWeight: 400 }}>(optional)</span>
          </label>
          <input
            id="sr-domain"
            className="input"
            type="text"
            placeholder="your-domain.com — leave blank for a vendor subdomain"
            value={form.domain}
            onChange={(e) => setField('domain', e.target.value)}
            disabled={saving}
          />
        </div>

        {/* Notes (optional) */}
        <div>
          <label htmlFor="sr-notes" style={{ display: 'block', marginBottom: '0.25rem', fontSize: '0.875rem', fontWeight: 600 }}>
            Additional notes{' '}
            <span style={{ color: 'var(--mm-color-text-secondary)', fontWeight: 400 }}>(optional)</span>
          </label>
          <textarea
            id="sr-notes"
            className="input"
            rows={4}
            placeholder="Expected audience size, special requirements, timeline..."
            value={form.notes}
            onChange={(e) => setField('notes', e.target.value)}
            disabled={saving}
            style={{ resize: 'vertical', fontFamily: 'inherit' }}
          />
        </div>

        <div>
          <button className="btn btn-primary" type="submit" disabled={saving}>
            {saving ? 'Sending…' : 'Submit request'}
          </button>
        </div>
      </form>
    </div>
  );
}
