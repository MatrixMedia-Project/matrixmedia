import { useState, useCallback } from 'react';

interface ShareButtonProps {
  streamId: string;
  /** Share grant id from a `?grant=` share link, carried into shared URLs. */
  grantId?: string;
}

/**
 * Share controls: copy link and show embed code.
 */
export function ShareButton({ streamId, grantId }: ShareButtonProps) {
  const [showEmbed, setShowEmbed] = useState(false);
  const [copied, setCopied] = useState(false);

  const grantQuery = grantId ? `?grant=${encodeURIComponent(grantId)}` : '';
  const watchUrl = `${window.location.origin}/_mm/viewer/watch/${encodeURIComponent(streamId)}${grantQuery}`;
  const embedUrl = `${window.location.origin}/_mm/viewer/embed/${encodeURIComponent(streamId)}${grantQuery}`;
  const embedCode = `<iframe src="${embedUrl}" width="400" height="200" frameborder="0" allow="autoplay; encrypted-media" allowfullscreen></iframe>`;

  const handleCopyLink = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(watchUrl);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // Fallback for contexts without clipboard API
      const textarea = document.createElement('textarea');
      textarea.value = watchUrl;
      document.body.appendChild(textarea);
      textarea.select();
      document.execCommand('copy');
      document.body.removeChild(textarea);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }
  }, [watchUrl]);

  const handleCopyEmbed = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(embedCode);
    } catch {
      // silent fallback
    }
  }, [embedCode]);

  return (
    <div className="mm-share">
      <div className="mm-share__buttons">
        <button className="mm-btn mm-btn--secondary" onClick={handleCopyLink}>
          {copied ? 'Copied!' : 'Copy Link'}
        </button>
        <button
          className="mm-btn mm-btn--secondary"
          onClick={() => setShowEmbed((s) => !s)}
        >
          {showEmbed ? 'Hide Embed' : 'Embed'}
        </button>
      </div>
      {showEmbed && (
        <div className="mm-share__embed">
          <textarea
            className="mm-share__embed-code"
            readOnly
            value={embedCode}
            rows={3}
            onClick={(e) => (e.target as HTMLTextAreaElement).select()}
          />
          <button className="mm-btn mm-btn--small" onClick={handleCopyEmbed}>
            Copy Embed Code
          </button>
        </div>
      )}
    </div>
  );
}
