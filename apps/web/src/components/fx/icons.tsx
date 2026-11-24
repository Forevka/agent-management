/*
 * The icons this app needs, inline.
 *
 * affectUI's components take an icon as a node and draw it at the size the control asks for, so
 * an icon set is not part of the design system — but a dependency on one would be a dependency
 * the orchestrator has to serve, and `index.html` says this page must work with no internet. So
 * they live here: one 24×24 stroke grid, `currentColor`, round caps, like the inline SVGs the
 * library's own components use for their spinners and chevrons.
 */

import type { SVGProps } from 'react'

type IconProps = SVGProps<SVGSVGElement>

function Icon({ children, ...rest }: IconProps) {
  return (
    <svg
      viewBox="0 0 24 24"
      width="1em"
      height="1em"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.7}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
      {...rest}
    >
      {children}
    </svg>
  )
}

export const WarningIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="M12 4.5 2.8 20h18.4L12 4.5Z" />
    <path d="M12 10.2v4.1" />
    <path d="M12 17.2h.01" />
  </Icon>
)

export const CheckIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="m4.5 12.8 4.7 4.5L19.5 6.8" />
  </Icon>
)

export const CheckCircleIcon = (p: IconProps) => (
  <Icon {...p}>
    <circle cx="12" cy="12" r="8.8" />
    <path d="m8.2 12.4 2.6 2.5 5-5.4" />
  </Icon>
)

export const InfoIcon = (p: IconProps) => (
  <Icon {...p}>
    <circle cx="12" cy="12" r="8.8" />
    <path d="M12 11v5.2" />
    <path d="M12 7.9h.01" />
  </Icon>
)

export const LockIcon = (p: IconProps) => (
  <Icon {...p}>
    <rect x="4.5" y="10.5" width="15" height="9.5" rx="2.4" />
    <path d="M8.2 10.5V7.8a3.8 3.8 0 0 1 7.6 0v2.7" />
  </Icon>
)

export const SearchIcon = (p: IconProps) => (
  <Icon {...p}>
    <circle cx="11" cy="11" r="6.8" />
    <path d="m16.2 16.2 3.6 3.6" />
  </Icon>
)

export const ArrowLeftIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="M19 12H5" />
    <path d="m11 6-6 6 6 6" />
  </Icon>
)

export const ExternalIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="M14 5h5v5" />
    <path d="M19 5l-7.5 7.5" />
    <path d="M18 14.5V18a1.8 1.8 0 0 1-1.8 1.8H6A1.8 1.8 0 0 1 4.2 18V7.8A1.8 1.8 0 0 1 6 6h3.5" />
  </Icon>
)

export const SunIcon = (p: IconProps) => (
  <Icon {...p}>
    <circle cx="12" cy="12" r="4.2" />
    <path d="M12 2.8v2.1M12 19.1v2.1M4.4 4.4l1.5 1.5M18.1 18.1l1.5 1.5M2.8 12h2.1M19.1 12h2.1M4.4 19.6l1.5-1.5M18.1 5.9l1.5-1.5" />
  </Icon>
)

export const MoonIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="M20 14.1A8.4 8.4 0 0 1 9.9 4a8.6 8.6 0 1 0 10.1 10.1Z" />
  </Icon>
)

export const AutoThemeIcon = (p: IconProps) => (
  <Icon {...p}>
    <rect x="3" y="4.5" width="18" height="12" rx="2.2" />
    <path d="M9 20h6" />
    <path d="M12 16.5V20" />
    <path d="M12 7.6a4.4 4.4 0 0 0 0 8.8Z" fill="currentColor" stroke="none" />
  </Icon>
)

export const RefreshIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="M20 11.5a8 8 0 1 0-2.6 6.3" />
    <path d="M20 5.5v6h-6" />
  </Icon>
)

/** An agent did this: the four-point spark the library uses for anything model-driven. */
export const SparkIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="M12 3.2c.9 4.3 1.6 5 5.9 5.9-4.3.9-5 1.6-5.9 5.9-.9-4.3-1.6-5-5.9-5.9 4.3-.9 5-1.6 5.9-5.9Z" />
    <path d="M18.2 15.1c.4 1.9.7 2.2 2.6 2.6-1.9.4-2.2.7-2.6 2.6-.4-1.9-.7-2.2-2.6-2.6 1.9-.4 2.2-.7 2.6-2.6Z" />
  </Icon>
)

/** Something attached to a task. */
export const PaperclipIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="m19.2 11.4-7.3 7.3a4.6 4.6 0 0 1-6.5-6.5l7.6-7.6a3.1 3.1 0 0 1 4.4 4.4l-7.5 7.5a1.5 1.5 0 0 1-2.2-2.2l6.9-6.9" />
  </Icon>
)

/** A file someone can download. */
export const FileIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="M13.8 3.5H7.2a1.8 1.8 0 0 0-1.8 1.8v13.4a1.8 1.8 0 0 0 1.8 1.8h9.6a1.8 1.8 0 0 0 1.8-1.8V8.3l-4.8-4.8Z" />
    <path d="M13.8 3.5v4.8h4.8" />
  </Icon>
)

/** What something cost: a coin. */
export const CoinIcon = (p: IconProps) => (
  <Icon {...p}>
    <circle cx="12" cy="12" r="8.4" />
    <path d="M14.6 9.4c-.5-.8-1.5-1.3-2.6-1.3-1.5 0-2.6.8-2.6 1.9 0 2.6 5.4 1.4 5.4 4 0 1.1-1.2 2-2.8 2-1.2 0-2.2-.5-2.7-1.3" />
    <path d="M12 6.6v1.5M12 15.9v1.5" />
  </Icon>
)

/** How many times a task has run: stacked sessions. */
export const SessionsIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="M12 3.6 3.4 8 12 12.4 20.6 8 12 3.6Z" />
    <path d="m3.4 12.6 8.6 4.4 8.6-4.4" />
    <path d="m3.4 17 8.6 4.4 8.6-4.4" />
  </Icon>
)

/** Checklist progress: how much of the plan is ticked off. */
export const MilestoneIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="M4 6.4h5" />
    <path d="M4 12h5" />
    <path d="M4 17.6h5" />
    <path d="m12.6 5.6 1.6 1.6 3-3" />
    <path d="m12.6 11.2 1.6 1.6 3-3" />
    <path d="m12.6 16.8 1.6 1.6 3-3" />
  </Icon>
)

/** The board view's two-column glyph, and the list view's lines. */
export const BoardIcon = (p: IconProps) => (
  <Icon {...p}>
    <rect x="3.6" y="4.4" width="6.2" height="15.2" rx="1.4" />
    <rect x="14.2" y="4.4" width="6.2" height="9.4" rx="1.4" />
  </Icon>
)

export const ListIcon = (p: IconProps) => (
  <Icon {...p}>
    <path d="M9 6.6h11" />
    <path d="M9 12h11" />
    <path d="M9 17.4h11" />
    <path d="M4.6 6.6h.01" />
    <path d="M4.6 12h.01" />
    <path d="M4.6 17.4h.01" />
  </Icon>
)
