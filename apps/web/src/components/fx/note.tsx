import type { ReactNode } from 'react'
import { CheckCircleIcon, InfoIcon, WarningIcon } from './icons'

/*
 * Note – a line of context in its own box: what went wrong, what is about to happen, what the
 * fleet did on the owner's behalf. affectUI has no callout, so this is ours, to the same
 * contract. The tone sets the colour pair (`--aui-x` / `--aui-x-soft`) and the icon, so a page
 * only has to say which kind of thing it is saying.
 */

export type NoteTone = 'info' | 'danger' | 'warning' | 'success'

export type NoteProps = {
  tone?: NoteTone
  title?: ReactNode
  children?: ReactNode
  /** A button or link belonging to the note, on its own line under the text. */
  action?: ReactNode
  className?: string
}

const ICON: Record<NoteTone, ReactNode> = {
  info: <InfoIcon />,
  danger: <WarningIcon />,
  warning: <WarningIcon />,
  success: <CheckCircleIcon />,
}

export function Note({ tone = 'info', title, children, action, className }: NoteProps) {
  return (
    <>
      <style href="fx-note" precedence="default">
        {css}
      </style>
      <div
        className={['fx-note', className].filter(Boolean).join(' ')}
        data-tone={tone}
        role={tone === 'danger' ? 'alert' : undefined}
      >
        <span className="fx-note__icon" aria-hidden="true">
          {ICON[tone]}
        </span>
        <div className="fx-note__body">
          {title && <strong className="fx-note__title">{title}</strong>}
          {children && <div className="fx-note__text">{children}</div>}
          {action && <div className="fx-note__action">{action}</div>}
        </div>
      </div>
    </>
  )
}

const css = /* css */ `
.fx-note {
  --fx-note-ink: var(--aui-info, #2456d6);
  --fx-note-bg: var(--aui-info-soft, #eaf1ff);

  display: flex;
  gap: 10px;
  padding: 11px 13px;
  background: var(--fx-note-bg);
  border: 1px solid color-mix(in srgb, var(--fx-note-ink) 22%, transparent);
  border-radius: var(--aui-radius, 10px);
  color: var(--aui-ink, #171717);
  font-size: 13.5px;
  line-height: 1.5;
}
.fx-note[data-tone="danger"] {
  --fx-note-ink: var(--aui-danger, #c4320a);
  --fx-note-bg: var(--aui-danger-soft, #fdecea);
}
.fx-note[data-tone="warning"] {
  --fx-note-ink: var(--aui-warning, #b25e00);
  --fx-note-bg: var(--aui-warning-soft, #fff1e0);
}
.fx-note[data-tone="success"] {
  --fx-note-ink: var(--aui-success, #1e8a4f);
  --fx-note-bg: var(--aui-success-soft, #e6f6ed);
}

.fx-note__icon {
  display: grid;
  place-items: center;
  flex: none;
  width: 18px;
  height: 20px;
  font-size: 16px;
  color: var(--fx-note-ink);
}
.fx-note__body {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
  overflow-wrap: anywhere;
}
.fx-note__title {
  font-weight: 600;
  color: var(--fx-note-ink);
}
.fx-note__action {
  margin-top: 4px;
}
`
