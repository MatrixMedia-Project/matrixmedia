interface PageHeaderProps {
  title: string;
  description?: string;
}

/**
 * Standard page header — h1 title + optional muted one-liner description.
 * Uses .page-header / .page-desc classes already in dashboard.css.
 */
export function PageHeader({ title, description }: PageHeaderProps) {
  return (
    <div className="page-header">
      <h1>{title}</h1>
      {description && <p className="page-desc">{description}</p>}
    </div>
  );
}
