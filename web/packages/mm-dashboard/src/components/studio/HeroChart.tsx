import type { ReactNode } from 'react';

interface HeroChartProps {
  title: string;
  caption?: string;
  /** Tab labels (metric picker or range picker). */
  tabs?: readonly string[];
  activeTab?: string;
  onTabChange?: (tab: string) => void;
  /** Extra page-agnostic control beside the tabs (e.g. an external link). */
  extraControl?: ReactNode;
  /** Optional note banner above the body. */
  note?: ReactNode;
  children: ReactNode;
}

/** Metric-picker tabs + content slot. Wraps Grafana d-solo iframes on the operator side. */
export function HeroChart({
  title,
  caption,
  tabs,
  activeTab,
  onTabChange,
  extraControl,
  note,
  children,
}: HeroChartProps) {
  return (
    <section className="mm-hero">
      <div className="mm-hero-head">
        <div>
          <div className="mm-hero-title">{title}</div>
          {caption && <div className="mm-hero-caption">{caption}</div>}
        </div>
        <div className="mm-hero-controls">
          {tabs && tabs.length > 0 && (
            <div className="mm-hero-tabs" role="tablist">
              {tabs.map((tab) => (
                <button
                  key={tab}
                  role="tab"
                  aria-selected={tab === activeTab}
                  className={tab === activeTab ? 'mm-hero-tab mm-hero-tab--active' : 'mm-hero-tab'}
                  onClick={() => onTabChange?.(tab)}
                >
                  {tab}
                </button>
              ))}
            </div>
          )}
          {extraControl}
        </div>
      </div>
      <div className="mm-hero-body">
        {note && <div className="mm-hero-note">{note}</div>}
        {children}
      </div>
    </section>
  );
}
