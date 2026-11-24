import { createContext, useContext, type ReactNode } from 'react'

/*
 * Project colours – everything that belongs to a project carries its hue, so the owner tells the
 * projects apart at a glance (DAP in pale blue, say). The hue is the one colour in this app that
 * is not status (DESIGN.md): it comes only from the `--fx-hue-*` tokens in `aui.css` and only
 * ever shows as a dot, a stripe or a soft tag, never as text or a filled control.
 *
 * The projects come from the overview the shell already polls, handed down through a context, so
 * a page that names a project by its slug needs no request of its own to colour it.
 */

export type ProjectInfo = { slug: string; name: string; color: string }

/** The palette, in the order the picker shows it (the orchestrator's `admin::PROJECT_COLORS`). */
export const PROJECT_COLORS = [
  'sky', 'blue', 'indigo', 'violet', 'pink', 'rose', 'orange', 'amber', 'lime', 'green', 'teal', 'slate',
] as const

const Projects = createContext<Map<string, ProjectInfo>>(new Map())

export function ProjectsProvider({ projects, children }: { projects: ProjectInfo[]; children: ReactNode }) {
  return <Projects.Provider value={new Map(projects.map((p) => [p.slug, p]))}>{children}</Projects.Provider>
}

/** The project of this slug, as the shell knows it (null before the first overview arrives). */
export function useProject(slug: string | null | undefined): ProjectInfo | null {
  const map = useContext(Projects)
  return (slug && map.get(slug)) || null
}

/** For lists: a function from a slug to its project's colour name (undefined while unknown). */
export function useProjectColors(): (slug: string | null | undefined) => string | undefined {
  const map = useContext(Projects)
  return (slug) => (slug ? map.get(slug)?.color : undefined)
}

/** The attribute that gives an element its project's hue (`--fx-project`, `--fx-project-soft`). */
export function useProjectAttrs(slug: string | null | undefined): { 'data-project'?: string } {
  const p = useProject(slug)
  return p ? { 'data-project': p.color } : {}
}

/** A small dot in the project's hue, with the project's name for screen readers and on hover. */
export function ProjectDot({ slug, size = 8 }: { slug: string | null | undefined; size?: number }) {
  const p = useProject(slug)
  if (!slug) return null
  return (
    <>
      <style href="fx-project" precedence="default">
        {css}
      </style>
      <span
        className="fx-project-dot"
        data-project={p?.color}
        style={{ width: size, height: size }}
        role="img"
        aria-label={p?.name ?? slug}
        title={p?.name ?? slug}
      />
    </>
  )
}

/** The project's name on a soft tag of its hue, with its dot. Text stays ink. */
export function ProjectTag({ slug, children }: { slug: string | null | undefined; children?: ReactNode }) {
  const p = useProject(slug)
  if (!slug) return null
  return (
    <>
      <style href="fx-project" precedence="default">
        {css}
      </style>
      <span className="fx-project-tag" data-project={p?.color}>
        <span className="fx-project-dot" aria-hidden="true" />
        {children ?? p?.name ?? slug}
      </span>
    </>
  )
}

/**
 * The owner's choice of a project's colour: a row of swatches, the chosen one ringed. Each swatch
 * is a real radio button, so the arrow keys move along the row.
 */
export function ProjectColorPicker({
  value,
  name,
  onPick,
  disabled,
}: {
  value: string
  name: string
  onPick: (color: string) => void
  disabled?: boolean
}) {
  return (
    <>
      <style href="fx-project" precedence="default">
        {css}
      </style>
      <div className="fx-project-picker" role="radiogroup" aria-label="Project colour">
        {PROJECT_COLORS.map((c) => (
          <label key={c} className="fx-project-swatch" data-project={c} title={c}>
            <input
              type="radio"
              name={name}
              value={c}
              checked={value === c}
              disabled={disabled}
              onChange={() => onPick(c)}
              aria-label={c}
            />
          </label>
        ))}
      </div>
    </>
  )
}

const css = /* css */ `
.fx-project-dot {
  display: inline-block;
  flex: none;
  width: 8px;
  height: 8px;
  border-radius: 999px;
  background: var(--fx-project, var(--aui-border-strong, #d4d4d4));
}
.fx-project-tag {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  max-width: 100%;
  padding: 1px 8px 1px 7px;
  border-radius: 999px;
  background: var(--fx-project-soft, var(--aui-subtle, #f4f4f5));
  color: var(--aui-ink, #171717);
  font-size: 12px;
  font-weight: 500;
  line-height: 18px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.fx-project-picker {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}
.fx-project-swatch {
  position: relative;
  width: 24px;
  height: 24px;
  border-radius: 999px;
  background: var(--fx-project);
  cursor: pointer;
  box-shadow: 0 0 0 2px var(--aui-surface, #ffffff);
}
.fx-project-swatch input {
  position: absolute;
  inset: 0;
  margin: 0;
  opacity: 0;
  cursor: pointer;
}
.fx-project-swatch:has(input:checked) {
  box-shadow:
    0 0 0 2px var(--aui-surface, #ffffff),
    0 0 0 4px var(--fx-project);
}
.fx-project-swatch:has(input:focus-visible) {
  outline: 2px solid var(--aui-ink, #171717);
  outline-offset: 4px;
}
.fx-project-swatch:has(input:disabled) {
  cursor: default;
  opacity: 0.6;
}
`
