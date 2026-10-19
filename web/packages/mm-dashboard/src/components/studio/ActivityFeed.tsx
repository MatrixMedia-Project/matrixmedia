import { feedDotClass, formatRelativeTime, type FeedItem } from './feedUtils';

interface ActivityFeedProps {
  items: readonly FeedItem[];
  /** Defaults to Date.now() at render. */
  now?: number;
  emptyTitle?: string;
  emptyHint?: string;
}

/** Relative-time list with severity dots. Newest first is the caller's job. */
export function ActivityFeed({ items, now, emptyTitle, emptyHint }: ActivityFeedProps) {
  const at = now ?? Date.now();
  if (items.length === 0) {
    return (
      <div className="mm-empty">
        <div className="mm-empty-title">{emptyTitle ?? 'Nothing yet'}</div>
        {emptyHint && <div className="mm-empty-hint">{emptyHint}</div>}
      </div>
    );
  }
  return (
    <ul className="mm-feed">
      {items.map((item) => (
        <li key={item.id} className="mm-feed-item">
          <span className={feedDotClass(item.severity)} aria-hidden="true" />
          <div className="mm-feed-body">
            <div className="mm-feed-text">{item.text}</div>
            <div className="mm-feed-time">{formatRelativeTime(item.at, at)}</div>
          </div>
        </li>
      ))}
    </ul>
  );
}
