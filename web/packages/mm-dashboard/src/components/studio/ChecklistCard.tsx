import { Link } from 'react-router-dom';
import { checklistProgress, type ChecklistTask } from './checklist';

interface ChecklistCardProps {
  tasks: readonly ChecklistTask[];
  actionHref?: string;
  actionLabel?: string;
}

/** Get-started checklist with a progress bar. Links stay client-side. */
export function ChecklistCard({ tasks, actionHref, actionLabel }: ChecklistCardProps) {
  const progress = checklistProgress(tasks);
  return (
    <div>
      <ul className="mm-check-list">
        {tasks.map((task) => (
          <li
            key={task.id}
            className={task.done ? 'mm-check-item mm-check-item--done' : 'mm-check-item'}
          >
            <span className="mm-check-box" aria-hidden="true">
              ✓
            </span>
            <span className="mm-check-label">
              {task.href ? (
                <Link to={task.href} className="mm-check-card-link">
                  {task.label}
                </Link>
              ) : (
                task.label
              )}
              {task.hint && <span className="mm-check-hint">{task.hint}</span>}
            </span>
          </li>
        ))}
      </ul>
      <div
        className="mm-check-progress"
        role="progressbar"
        aria-valuenow={progress.pct}
        aria-valuemin={0}
        aria-valuemax={100}
      >
        <div className="mm-check-progress-bar" data-decile={Math.round(progress.pct / 10) * 10} />
      </div>
      <div className="mm-check-count">
        {progress.done} of {progress.total} complete
        {actionHref && actionLabel && (
          <>
            {' · '}
            <Link to={actionHref} className="mm-check-card-link">
              {actionLabel}
            </Link>
          </>
        )}
      </div>
    </div>
  );
}
