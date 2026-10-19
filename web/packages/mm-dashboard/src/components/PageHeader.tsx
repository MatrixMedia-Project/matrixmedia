import type { ReactNode } from 'react';

interface PageHeaderProps {
  title: string;
  description?: ReactNode;
  /** Small kicker line above the title (e.g. a section eyebrow). */
  caption?: string;
  /** Page-agnostic action slot rendered beside the title block. */
  actions?: ReactNode;
}

/**
 * Standard page header — h1 title + optional muted one-liner description.
 * Uses .page-header / .page-desc classes already in dashboard.css.
 */
export function PageHeader({ title, description, caption, actions }: PageHeaderProps) {
  return (
    <div className="page-header">
      {caption && <div className="mm-section-title">{caption}</div>}
      <h1>{title}</h1>
      {description && <p className="page-desc">{description}</p>}
      {actions}
    </div>
  );
}
