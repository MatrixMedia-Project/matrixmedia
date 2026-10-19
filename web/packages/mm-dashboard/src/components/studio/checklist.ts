/** Setup-checklist progress helper (pure, vitest-covered). */

export interface ChecklistTask {
  id: string;
  label: string;
  hint?: string;
  done: boolean;
  href?: string;
}

export interface ChecklistProgress {
  done: number;
  total: number;
  /** Whole percent, 0-100. Empty lists read as complete (nothing to do). */
  pct: number;
}

export function checklistProgress(tasks: readonly ChecklistTask[]): ChecklistProgress {
  const total = tasks.length;
  if (total === 0) return { done: 0, total: 0, pct: 100 };
  const done = tasks.filter((t) => t.done).length;
  return { done, total, pct: Math.round((done / total) * 100) };
}
