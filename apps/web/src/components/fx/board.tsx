import { useEffect } from 'react'
import type { ReactNode } from 'react'
import { Link } from 'react-router-dom'

/*
 * Board – the fleet's work as columns of cards.
 *
 * affectUI has no kanban in the free set, so this is ours, to the same contract: one file, styles
 * injected beside it, `data-*` for variants, every colour from an `--aui-*` token, dark mode by
 * ancestor attribute, reduced motion honoured.
 *
 * It is deliberately a *reading* board, not a planning one: the cards cannot be dragged, because
 * a column here is the task's real state in the fleet, not a label someone chose. Moving a card
 * by hand would be a lie — the way to move work is to answer it, stop it, or let the agent run.
 */

export type BoardTone = 'default' | 'info' | 'success' | 'warning' | 'danger'

export function Board({ children }: { children: ReactNode }) {
  // The board breaks out of the page's reading column to the width of the window, which in CSS
  // means `100vw` — and `100vw` counts the scrollbar gutter that `html` keeps reserved, so using
  // it raw would push the page sideways by that much. The gutter is the only thing CSS cannot
  // tell us, so it is measured once and published as a variable.
  useEffect(() => {
    const measure = () => {
      const sb = window.innerWidth - document.documentElement.clientWidth
      document.documentElement.style.setProperty('--fx-sb', `${Math.max(0, sb)}px`)
    }
    measure()
    window.addEventListener('resize', measure)
    return () => window.removeEventListener('resize', measure)
  }, [])
  return (
    <>
      <style href="fx-board" precedence="default">
        {css}
      </style>
      <div className="fx-board">{children}</div>
    </>
  )
}

export function BoardColumn({
  title,
  count,
  tone = 'default',
  icon,
  children,
  empty,
  collapsed,
  onToggle,
}: {
  title: ReactNode
  count: number
  tone?: BoardTone
  icon?: ReactNode
  children?: ReactNode
  empty?: ReactNode
  /** Folded to a vertical strip, so the columns being read get the width. */
  collapsed?: boolean
  onToggle?: () => void
}) {
  // Collapsed, the whole column is the button — a 44px strip is a comfortable target, and there
  // is nothing else in it to click. Open, only the chevron toggles, so a header can still be
  // selected and read.
  const label = `${collapsed ? 'Expand' : 'Collapse'} ${typeof title === 'string' ? title : 'column'}`
  if (collapsed) {
    return (
      <section className="fx-board__col" data-tone={tone} data-collapsed="">
        <button type="button" className="fx-board__strip" onClick={onToggle} title={label} aria-label={label}>
          <span className="fx-board__count">{count}</span>
          <span className="fx-board__strip-title">{title}</span>
          {icon && <span className="fx-board__head-icon">{icon}</span>}
        </button>
      </section>
    )
  }
  return (
    <section className="fx-board__col" data-tone={tone}>
      <header className="fx-board__head">
        {icon && <span className="fx-board__head-icon">{icon}</span>}
        <h2 className="fx-board__title">{title}</h2>
        <span className="fx-board__count">{count}</span>
        {onToggle && (
          <button
            type="button"
            className="fx-board__fold"
            onClick={onToggle}
            title={label}
            aria-label={label}
          >
            <svg viewBox="0 0 24 24" width="1em" height="1em" fill="none" stroke="currentColor" strokeWidth={2} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
              <path d="m14.5 6.5-5 5.5 5 5.5" />
            </svg>
          </button>
        )}
      </header>
      <div className="fx-board__cards">
        {count === 0 ? <p className="fx-board__empty">{empty ?? 'Nothing here'}</p> : children}
      </div>
    </section>
  )
}

export type BoardCardProps = {
  to: string
  title: ReactNode
  /** A small mark before the title, tinted by the card's tone. */
  mark?: ReactNode
  /** Two lines at most; the rest is clamped away. */
  text?: ReactNode
  /** The one line that says where this stands: an icon, a word, and usually a duration. */
  state: ReactNode
  tone?: BoardTone
  /** The small counts along the bottom. */
  facts?: { icon: ReactNode; value: ReactNode; title: string }[]
}

export function BoardCard({ to, title, mark, text, state, tone = 'default', facts }: BoardCardProps) {
  return (
    <Link className="fx-card" data-tone={tone} to={to}>
      <span className="fx-card__title">
        {mark && <span className="fx-card__mark">{mark}</span>}
        {title}
      </span>
      {text && <span className="fx-card__text">{text}</span>}
      <span className="fx-card__state">{state}</span>
      {facts && facts.length > 0 && (
        <span className="fx-card__facts">
          {facts.map((f, i) => (
            <span className="fx-card__fact" key={i} title={f.title}>
              <span className="fx-card__fact-icon">{f.icon}</span>
              {f.value}
            </span>
          ))}
        </span>
      )}
    </Link>
  )
}

/** The dot or spinner that leads a card's state line. */
export function BoardDot({ tone = 'default', spin }: { tone?: BoardTone; spin?: boolean }) {
  return <span className="fx-card__dot" data-tone={tone} data-spin={spin || undefined} />
}

const css = /* css */ `
.fx-board {
  --fx-col-w: 20.5rem;
  --fx-board-gutter: 20px;
  display: flex;
  gap: 16px;
  align-items: flex-start;
  overflow-x: auto;
  /* A board wants the window, not the page's reading column: seven states side by side do not
     fit in 1200px, and a column half-hidden behind the fold is a column nobody reads. So it
     breaks out to the full width and scrolls from there, keeping a gutter of its own.
     (No backticks in here: this block is a JS template literal.) */
  width: calc(100vw - var(--fx-sb, 0px));
  max-width: calc(100vw - var(--fx-sb, 0px));
  margin-inline: calc(50% - 50vw + var(--fx-sb, 0px) / 2);
  padding: 2px var(--fx-board-gutter) 14px;
  scrollbar-width: thin;
  scrollbar-color: var(--aui-border-strong, #d4d4d8) transparent;
  /* Scroll-padding so a column never stops flush against the edge. */
  scroll-padding-inline: var(--fx-board-gutter);
}

/* WebKit needs its own rules; without them the bar under the board is a heavy slab the width
   of the window, which is the loudest thing on a quiet page. */
.fx-board::-webkit-scrollbar {
  height: 10px;
}

.fx-board::-webkit-scrollbar-track {
  background: transparent;
}

.fx-board::-webkit-scrollbar-thumb {
  background: var(--aui-border-strong, #d4d4d8);
  border: 3px solid transparent;
  background-clip: content-box;
  border-radius: 999px;
}

.fx-board::-webkit-scrollbar-thumb:hover {
  background: var(--aui-muted, #737373);
  background-clip: content-box;
}

.fx-board__col {
  flex: 0 0 var(--fx-col-w);
  display: flex;
  flex-direction: column;
  gap: 10px;
  min-width: 0;
  transition: flex-basis 200ms var(--aui-ease, cubic-bezier(0.22, 1, 0.36, 1));
}

/* Folded: a narrow strip, its title reading bottom-to-top. The cards are not hidden with a
   display rule — the column does not render them at all, so folding a long column costs
   nothing. (No backticks in here: this block is a JS template literal.) */
.fx-board__col[data-collapsed] {
  flex: 0 0 44px;
}

.fx-board__strip {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 10px;
  width: 44px;
  padding: 10px 0 14px;
  border: 1px solid var(--aui-border, #e5e5e5);
  border-radius: var(--aui-radius, 10px);
  background: var(--aui-subtle, #f0f0f0);
  color: var(--fx-col-ink, var(--aui-muted, #737373));
  font: inherit;
  cursor: pointer;
  transition: background 140ms ease, border-color 140ms ease;
}

.fx-board__strip:hover {
  border-color: var(--aui-border-strong, #d4d4d8);
  background: var(--aui-surface, #ffffff);
}

.fx-board__strip:focus-visible {
  outline: 2px solid var(--aui-focus, rgba(0, 96, 226, 0.7));
  outline-offset: 2px;
}

.fx-board__strip-title {
  /* Sideways, reading up, so the eye travels the strip the way it travels a spine. */
  writing-mode: vertical-rl;
  transform: rotate(180deg);
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-height: 11rem;
}

.fx-board__fold {
  flex: 0 0 auto;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  margin-right: -4px;
  padding: 0;
  border: 0;
  border-radius: 6px;
  background: none;
  color: inherit;
  font-size: 15px;
  cursor: pointer;
  opacity: 0.55;
  transition: opacity 140ms ease, background 140ms ease;
}

.fx-board__fold:hover {
  opacity: 1;
  background: var(--aui-surface, #ffffff);
}

.fx-board__fold:focus-visible {
  outline: 2px solid var(--aui-focus, rgba(0, 96, 226, 0.7));
  outline-offset: 1px;
  opacity: 1;
}

.fx-board__head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 12px;
  border: 1px solid var(--aui-border, #e5e5e5);
  border-radius: var(--aui-radius, 10px);
  background: var(--aui-subtle, #f0f0f0);
  color: var(--aui-muted, #737373);
}

.fx-board__head-icon {
  display: inline-flex;
  font-size: 15px;
  color: var(--fx-col-ink, var(--aui-muted, #737373));
}

.fx-board__col[data-tone='danger'] { --fx-col-ink: var(--aui-danger, #c4320a); }
.fx-board__col[data-tone='warning'] { --fx-col-ink: var(--aui-warning, #b25e00); }
.fx-board__col[data-tone='success'] { --fx-col-ink: var(--aui-success, #1e8a4f); }
.fx-board__col[data-tone='info'] { --fx-col-ink: var(--aui-info, #2456d6); }

.fx-board__title {
  flex: 1 1 auto;
  margin: 0;
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: inherit;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.fx-board__count {
  flex: 0 0 auto;
  min-width: 20px;
  padding: 0 6px;
  border-radius: 999px;
  background: var(--aui-surface, #fff);
  border: 1px solid var(--aui-border, #e5e5e5);
  font-size: 11px;
  font-weight: 600;
  line-height: 18px;
  text-align: center;
  color: var(--aui-muted, #737373);
  font-variant-numeric: tabular-nums;
}

.fx-board__cards {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.fx-board__empty {
  margin: 0;
  padding: 14px 12px;
  border: 1px dashed var(--aui-border, #e5e5e5);
  border-radius: var(--aui-radius, 10px);
  font-size: 12px;
  color: var(--aui-muted, #737373);
}

/* ---------- card ---------- */

.fx-card {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 12px;
  border: 1px solid var(--aui-border, #e5e5e5);
  border-radius: var(--aui-radius, 10px);
  background: var(--aui-surface, #ffffff);
  box-shadow: var(--aui-shadow-card, 0 1px 2px rgba(0, 0, 0, 0.06));
  color: inherit;
  text-decoration: none;
  transition:
    transform 160ms var(--aui-ease, cubic-bezier(0.22, 1, 0.36, 1)),
    box-shadow 160ms var(--aui-ease, cubic-bezier(0.22, 1, 0.36, 1)),
    border-color 160ms ease;
}

.fx-card:hover {
  transform: translateY(-1px);
  border-color: var(--aui-border-strong, #d4d4d8);
  box-shadow: var(--aui-shadow-popover, 0 0 0 1px rgba(0, 0, 0, 0.06), 0 12px 32px rgba(0, 0, 0, 0.12));
}

.fx-card:focus-visible {
  outline: 2px solid var(--aui-focus, rgba(0, 96, 226, 0.7));
  outline-offset: 2px;
}

.fx-card__title {
  font-size: 13.5px;
  font-weight: 600;
  line-height: 1.35;
  /* A long ticket title must not push the card wider than its column. */
  overflow-wrap: anywhere;
}

/* The mark carries the card's tone, so a column of cards reads at a glance even before the
   words: it is the only colour on the card besides the state line. */
.fx-card__mark {
  display: inline-flex;
  vertical-align: -2px;
  margin-right: 6px;
  font-size: 13px;
  color: var(--aui-muted, #737373);
}

.fx-card[data-tone='danger'] .fx-card__mark { color: var(--aui-danger, #c4320a); }
.fx-card[data-tone='warning'] .fx-card__mark { color: var(--aui-warning, #b25e00); }
.fx-card[data-tone='success'] .fx-card__mark { color: var(--aui-success, #1e8a4f); }
.fx-card[data-tone='info'] .fx-card__mark { color: var(--aui-info, #2456d6); }

.fx-card__text {
  font-size: 12.5px;
  line-height: 1.45;
  color: var(--aui-muted, #737373);
  display: -webkit-box;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 2;
  line-clamp: 2;
  overflow: hidden;
  overflow-wrap: anywhere;
}

.fx-card__state {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 2px;
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  color: var(--aui-muted, #737373);
}

.fx-card[data-tone='danger'] .fx-card__state { color: var(--aui-danger, #c4320a); }
.fx-card[data-tone='warning'] .fx-card__state { color: var(--aui-warning, #b25e00); }
.fx-card[data-tone='success'] .fx-card__state { color: var(--aui-success, #1e8a4f); }
.fx-card[data-tone='info'] .fx-card__state { color: var(--aui-info, #2456d6); }

.fx-card__dot {
  flex: 0 0 auto;
  width: 8px;
  height: 8px;
  border-radius: 999px;
  background: currentColor;
}

/* Running work pulses, so a glance tells a live session from a finished one. */
.fx-card__dot[data-spin] {
  width: 10px;
  height: 10px;
  background: none;
  border: 1.6px solid currentColor;
  border-top-color: transparent;
  animation: fx-card-spin 720ms linear infinite;
}

@keyframes fx-card-spin {
  to { transform: rotate(360deg); }
}

.fx-card__facts {
  display: flex;
  gap: 14px;
  margin-top: 2px;
  padding-top: 9px;
  border-top: 1px solid var(--aui-border, #e5e5e5);
  font-size: 11.5px;
  color: var(--aui-muted, #737373);
  font-variant-numeric: tabular-nums;
}

.fx-card__fact {
  display: inline-flex;
  align-items: center;
  gap: 5px;
}

.fx-card__fact-icon {
  display: inline-flex;
  font-size: 13px;
  opacity: 0.8;
}

@media (prefers-reduced-motion: reduce) {
  .fx-card { transition: none; }
  .fx-card:hover { transform: none; }
  .fx-card__dot[data-spin] { animation: none; }
  .fx-board__col { transition: none; }
}

:where(.dark, [data-theme='dark']) .fx-board__head,
:where(.dark, [data-theme='dark']) .fx-board__strip {
  background: color-mix(in srgb, var(--aui-surface, #19191b) 70%, transparent);
}

:where(.dark, [data-theme='dark']) .fx-board__strip:hover,
:where(.dark, [data-theme='dark']) .fx-board__fold:hover {
  background: var(--aui-surface, #19191b);
}
`
