import { NavLink, Outlet } from 'react-router-dom'
import { ThemeToggle } from '@/components/fx/theme'

/*
 * The frame: a sticky header with the nav, and the page under it.
 *
 * One place decides the page width and the rhythm (see `fx-page` in layout.tsx), so every page
 * lines up. The header is written like a component of the library: its own styles, its own
 * variables reading `--aui-*`, and a dark rule keyed on an ancestor rather than a media query.
 */

const NAV = [
  ['/', 'Board'],
  ['/tasks', 'Tasks'],
  ['/sessions', 'Sessions'],
  ['/projects', 'Projects'],
  ['/inbox', 'Inbox'],
  ['/habits', 'Habits'],
  ['/retro', 'Retro'],
  ['/people', 'People'],
  ['/agents', 'Agents'],
  ['/nodes', 'Nodes'],
  ['/kit', 'Kit'],
  ['/secrets', 'Secrets'],
] as const

export function Shell() {
  return (
    <>
      <style href="fx-shell" precedence="default">
        {css}
      </style>
      <header className="fx-header">
        <div className="fx-header__inner">
          <NavLink to="/" className="fx-brand" aria-label="Fleet, the board">
            <span className="fx-brand__mark" aria-hidden="true">
              F
            </span>
            <span className="fx-brand__name">Fleet</span>
          </NavLink>
          <nav className="fx-nav" aria-label="Sections">
            {NAV.map(([to, label]) => (
              // `end` so "Board" is only current on "/" itself.
              <NavLink key={to} to={to} end={to === '/'} className="fx-nav__link">
                {label}
              </NavLink>
            ))}
          </nav>
          <div className="fx-header__end">
            <ThemeToggle />
          </div>
        </div>
      </header>
      <Outlet />
    </>
  )
}

const css = /* css */ `
.fx-header {
  --fx-hdr-surface: color-mix(in srgb, var(--aui-surface, #fff) 82%, transparent);
  --fx-hdr-line: var(--aui-border, #e5e5e5);
  --fx-hdr-ink: var(--aui-ink, #171717);
  --fx-hdr-muted: var(--aui-muted, #737373);
  --fx-hdr-hover: var(--aui-subtle, #f0f0f0);

  position: sticky;
  top: 0;
  z-index: 20;
  background: var(--fx-hdr-surface);
  border-bottom: 1px solid var(--fx-hdr-line);
  /* The page scrolls under the header, so it is translucent rather than opaque. */
  backdrop-filter: saturate(150%) blur(10px);
  -webkit-backdrop-filter: saturate(150%) blur(10px);
}

.fx-header__inner {
  display: flex;
  align-items: center;
  gap: 16px;
  height: var(--fx-header-height, 56px);
  max-width: var(--fx-page-width, 1200px);
  margin: 0 auto;
  padding: 0 20px;
}
@media (max-width: 640px) {
  .fx-header__inner {
    padding: 0 16px;
    gap: 10px;
  }
}

.fx-brand {
  display: inline-flex;
  align-items: center;
  gap: 9px;
  flex: none;
  color: var(--fx-hdr-ink);
  font-size: 14px;
  font-weight: 600;
  letter-spacing: -0.02em;
  text-decoration: none;
  border-radius: var(--aui-radius-sm, 7px);
}
.fx-brand__mark {
  display: grid;
  place-items: center;
  width: 24px;
  height: 24px;
  border-radius: 7px;
  background: linear-gradient(var(--aui-ink, #171717), color-mix(in srgb, var(--aui-ink, #171717) 80%, #000));
  color: var(--aui-surface, #fff);
  font-size: 13px;
  font-weight: 700;
  line-height: 1;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.18);
}
@media (max-width: 480px) {
  .fx-brand__name {
    display: none;
  }
}

.fx-nav {
  display: flex;
  align-items: center;
  gap: 2px;
  min-width: 0;
  flex: 1;
  /* On a phone the nav scrolls sideways instead of wrapping into three lines. */
  overflow-x: auto;
  scrollbar-width: none;
  -ms-overflow-style: none;
  scroll-snap-type: x proximity;
  mask-image: linear-gradient(90deg, transparent, #000 12px, #000 calc(100% - 12px), transparent);
}
.fx-nav::-webkit-scrollbar {
  display: none;
}

.fx-nav__link {
  flex: none;
  padding: 6px 10px;
  border-radius: var(--aui-radius-sm, 7px);
  color: var(--fx-hdr-muted);
  font-size: 13px;
  font-weight: 500;
  text-decoration: none;
  white-space: nowrap;
  scroll-snap-align: center;
  transition:
    color 140ms ease,
    background-color 140ms ease;
}
.fx-nav__link:hover {
  color: var(--fx-hdr-ink);
  background: var(--fx-hdr-hover);
}
.fx-nav__link[aria-current="page"] {
  color: var(--aui-on-primary, #fff);
  background: var(--aui-primary, #171717);
}

.fx-header__end {
  display: flex;
  align-items: center;
  gap: 6px;
  flex: none;
}

:where(.dark, [data-theme="dark"]) .fx-header {
  --fx-hdr-surface: color-mix(in srgb, var(--aui-surface, #19191b) 78%, transparent);
  --fx-hdr-line: var(--aui-border, #2a2a2e);
  --fx-hdr-ink: var(--aui-ink, #ededed);
  --fx-hdr-muted: var(--aui-muted, #8b8b92);
  --fx-hdr-hover: rgba(255, 255, 255, 0.06);
}
:where(.dark, [data-theme="dark"]) .fx-brand__mark {
  background: linear-gradient(#f4f4f5, #e4e4e7);
  color: #121213;
}
`
