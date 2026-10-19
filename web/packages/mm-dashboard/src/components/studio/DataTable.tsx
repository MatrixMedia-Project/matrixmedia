import { Children, type ReactNode } from 'react';

interface DataTableProps {
  columns: readonly string[];
  /**
   * Optional per-column header override (e.g. a select-all checkbox for a
   * label-less column). Falls back to the column label when absent.
   */
  headerCells?: readonly ReactNode[];
  /** Zero-based index of the sorted column, or null for unsorted. */
  sortColumn?: number | null;
  sortDirection?: 'asc' | 'desc';
  onSort?: (column: number) => void;
  emptyTitle: string;
  emptyHint?: string;
  emptyAction?: ReactNode;
  children: ReactNode;
}

/**
 * Dense sortable table shell. Pages keep their fetch/mutation hooks and render
 * their own `<tr>` rows as children; sorting stays page-owned via onSort.
 */
export function DataTable({
  columns,
  headerCells,
  sortColumn,
  sortDirection,
  onSort,
  emptyTitle,
  emptyHint,
  emptyAction,
  children,
}: DataTableProps) {
  const empty = Children.count(children) === 0;
  return (
    <div className="mm-table-wrap">
      <table className="mm-table">
        <thead>
          <tr>
            {columns.map((header, i) => {
              const sortable = onSort !== undefined;
              const active = sortColumn === i;
              return (
                <th
                  key={`${i}-${header}`}
                  className={sortable ? 'mm-table-sortable' : undefined}
                  aria-sort={active ? (sortDirection === 'desc' ? 'descending' : 'ascending') : undefined}
                  onClick={sortable ? () => onSort(i) : undefined}
                >
                  {headerCells?.[i] ?? header}
                  {active ? (sortDirection === 'desc' ? ' ▾' : ' ▴') : ''}
                </th>
              );
            })}
          </tr>
        </thead>
        <tbody>
          {empty ? (
            <tr>
              <td colSpan={Math.max(1, columns.length)} className="mm-table-empty">
                <div className="mm-empty-title">{emptyTitle}</div>
                {emptyHint && <div className="mm-empty-hint">{emptyHint}</div>}
                {emptyAction}
              </td>
            </tr>
          ) : (
            children
          )}
        </tbody>
      </table>
    </div>
  );
}
