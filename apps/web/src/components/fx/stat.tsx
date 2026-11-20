import type { ReactNode } from 'react'
import { Link } from 'react-router-dom'

/*
 * Stat – one number, said once.
 *
 * affectUI's Stat Card is a Pro item, so this is ours, to the same contract. It is deliberately
 * quiet: a label, the number, and at most one line under it. A number worth a colour says so with
 * `tone`, and a number worth looking into carries `to` and becomes a link to the page that
 * explains it — a figure on a dashboard that cannot be opened is a dead end.
 */

export type StatTone = 'default' | 'info' | 'success' | 'warning' | 'danger'

export type StatProps = {
  label: ReactNode
  value: ReactNode
  hint?: ReactNode
  tone?: StatTone
  icon?: ReactNode
  /** Where the number can be looked into, as a router path. */
  to?: string
  /** Says the figure is still loading rather than genuinely zero. */
  loading?: boolean
}

export function Stat({ label, value, hint, tone = 'default', icon, to, loading }: StatProps) {
  const body = (
    <>
      <span className="fx-stat__label">
        {icon && <span className="fx-stat__icon">{icon}</span>}
        {label}
      </span>
      <span className="fx-stat__value" data-loading={loading || undefined}>
        {loading ? '—' : value}
      </span>
      {hint && <span className="fx-stat__hint">{hint}</span>}
    </>
  )
  return (
    <>
      <style href="fx-stat" precedence="default">
        {css}
      </style>
      {to ? (
        <Link className="fx-stat" data-tone={tone} data-interactive="" to={to}>
          {body}
        </Link>
      ) : (
        <div className="fx-stat" data-tone={tone}>
          {body}
        </div>
      )}
    </>
  )
}

const css = /* css */ `
.fx-stat {
  --fx-stat-surface: var(--aui-surface, #ffffff);
  --fx-stat-line: var(--aui-border, #e5e5e5);
  --fx-stat-ink: var(--aui-ink, #171717);
  --fx-stat-muted: var(--aui-muted, #737373);
  --fx-stat-accent: var(--aui-ink, #171717);

  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 14px 16px 16px;
  background: var(--fx-stat-surface);
  border: 1px solid var(--fx-stat-line);
  border-radius: var(--aui-radius-lg, 16px);
  box-shadow: var(--aui-shadow-card, 0 1px 2px rgba(0, 0, 0, 0.06));
  color: var(--fx-stat-ink);
  text-decoration: none;
  min-width: 0;
  transition:
    border-color 160ms ease,
    box-shadow 160ms ease,
    transform 160ms var(--aui-ease, cubic-bezier(0.22, 1, 0.36, 1));
}
.fx-stat[data-interactive]:hover {
  border-color: var(--aui-border-strong, #d4d4d8);
  box-shadow: var(--aui-shadow-popover, 0 8px 24px rgba(0, 0, 0, 0.08));
  transform: translateY(-1px);
}
.fx-stat[data-interactive]:active {
  transform: translateY(0);
}

.fx-stat__label {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  color: var(--fx-stat-muted);
  font-size: 12.5px;
  font-weight: 500;
  letter-spacing: 0;
}
.fx-stat__icon {
  display: grid;
  place-items: center;
  font-size: 14px;
  color: var(--fx-stat-accent);
}

.fx-stat__value {
  font-size: 27px;
  font-weight: 600;
  line-height: 1.1;
  letter-spacing: -0.03em;
  font-variant-numeric: tabular-nums;
  color: var(--fx-stat-accent);
  /* A number that lands after its label must not shove the hint around. */
  min-height: 1.1em;
}
.fx-stat__value[data-loading] {
  color: var(--fx-stat-muted);
}

.fx-stat__hint {
  color: var(--fx-stat-muted);
  font-size: 12.5px;
}

.fx-stat[data-tone="info"] { --fx-stat-accent: var(--aui-info, #2456d6); }
.fx-stat[data-tone="success"] { --fx-stat-accent: var(--aui-success, #1e8a4f); }
.fx-stat[data-tone="warning"] { --fx-stat-accent: var(--aui-warning, #b25e00); }
.fx-stat[data-tone="danger"] { --fx-stat-accent: var(--aui-danger, #c4320a); }

:where(.dark, [data-theme="dark"]) .fx-stat {
  --fx-stat-surface: var(--aui-surface, #19191b);
  --fx-stat-line: var(--aui-border, #2a2a2e);
  --fx-stat-ink: var(--aui-ink, #ededed);
  --fx-stat-muted: var(--aui-muted, #8b8b92);
  --fx-stat-accent: var(--aui-ink, #ededed);
}
`
