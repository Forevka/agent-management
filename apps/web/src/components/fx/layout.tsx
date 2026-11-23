import type { ComponentPropsWithRef, ElementType, ReactNode } from 'react'

/*
 * Layout and type, written to the same contract as the components in `components/ui`: one file,
 * its styles injected beside it, variants as `data-*`, and every colour from an `--aui-*` token.
 * These are the pieces a page is assembled from, so a page itself holds no styling at all.
 */

const SPACE = ['0', '1', '2', '3', '4', '5', '6'] as const
export type Space = (typeof SPACE)[number]

type Align = 'start' | 'center' | 'end' | 'baseline' | 'stretch'
type Justify = 'start' | 'center' | 'end' | 'between'

export type StackProps = ComponentPropsWithRef<'div'> & {
  gap?: Space
  align?: Align
  justify?: Justify
}

/** A column. The default gap is the one most of this app uses between blocks. */
export function Stack({ gap = '3', align, justify, className, ...rest }: StackProps) {
  return (
    <>
      <style href="fx-layout" precedence="default">
        {css}
      </style>
      <div
        className={join('fx-stack', className)}
        data-gap={space(gap)}
        data-align={align}
        data-justify={justify}
        {...rest}
      />
    </>
  )
}

export type RowProps = StackProps & { wrap?: boolean }

/** A row that wraps by default, because every row in this app has to survive a phone. */
export function Row({ gap = '2', align = 'center', justify, wrap = true, className, ...rest }: RowProps) {
  return (
    <>
      <style href="fx-layout" precedence="default">
        {css}
      </style>
      <div
        className={join('fx-row', className)}
        data-gap={space(gap)}
        data-align={align}
        data-justify={justify}
        data-wrap={wrap || undefined}
        {...rest}
      />
    </>
  )
}

export type GridProps = ComponentPropsWithRef<'div'> & {
  /** The narrowest a column may get before the grid drops to fewer of them. */
  min?: number
  gap?: Space
}

/** A grid that reflows by width rather than by breakpoint, so it needs no media queries. */
export function Grid({ min = 240, gap = '3', className, style, ...rest }: GridProps) {
  return (
    <>
      <style href="fx-layout" precedence="default">
        {css}
      </style>
      <div
        className={join('fx-grid', className)}
        data-gap={space(gap)}
        style={{ ['--fx-grid-min' as string]: `${min}px`, ...style }}
        {...rest}
      />
    </>
  )
}

export type TextProps<E extends ElementType> = {
  as?: E
  size?: '1' | '2' | '3'
  tone?: 'default' | 'muted' | 'danger' | 'success' | 'warning'
  weight?: 'regular' | 'medium' | 'strong'
  mono?: boolean
  /** A long id, path or URL wraps instead of stretching its container off the page. */
  breakAll?: boolean
  children?: ReactNode
} & Omit<ComponentPropsWithRef<E>, 'as' | 'size' | 'children'>

/** Body text. Three sizes, because a fourth has never been needed here. */
export function Text<E extends ElementType = 'span'>({
  as,
  size = '2',
  tone = 'default',
  weight = 'regular',
  mono,
  breakAll,
  className,
  ...rest
}: TextProps<E>) {
  const As = (as ?? 'span') as ElementType
  return (
    <>
      <style href="fx-layout" precedence="default">
        {css}
      </style>
      <As
        className={join('fx-text', className)}
        data-size={size}
        data-tone={tone}
        data-weight={weight}
        data-mono={mono || undefined}
        data-break={breakAll || undefined}
        {...rest}
      />
    </>
  )
}

export type HeadingProps = ComponentPropsWithRef<'h1'> & {
  as?: 'h1' | 'h2' | 'h3' | 'h4'
  size?: '1' | '2' | '3'
}

export function Heading({ as = 'h2', size = '2', className, ...rest }: HeadingProps) {
  const As = as as ElementType
  return (
    <>
      <style href="fx-layout" precedence="default">
        {css}
      </style>
      <As className={join('fx-heading', className)} data-size={size} {...rest} />
    </>
  )
}

export type SectionProps = {
  title?: ReactNode
  description?: ReactNode
  /** Buttons or filters belonging to the section, right-aligned beside its title. */
  action?: ReactNode
  children?: ReactNode
  className?: string
}

/** A titled block. Every page is a stack of these, which is what makes the pages line up. */
export function Section({ title, description, action, children, className }: SectionProps) {
  return (
    <>
      <style href="fx-layout" precedence="default">
        {css}
      </style>
      <section className={join('fx-section', className)}>
        {(title || action) && (
          <header className="fx-section__head">
            <div className="fx-section__titles">
              {title && (
                <Heading as="h2" size="1">
                  {title}
                </Heading>
              )}
              {description && (
                <Text as="p" size="1" tone="muted" className="fx-section__desc">
                  {description}
                </Text>
              )}
            </div>
            {action && <div className="fx-section__action">{action}</div>}
          </header>
        )}
        {children}
      </section>
    </>
  )
}

/**
 * The page frame: one place decides the width and the rhythm, so every page agrees. A `div`: the
 * app shell's panel around it is the page's one `<main>`.
 */
/**
 * `fill`: the page is exactly as tall as the panel it sits in, so a last child that grows into the
 * room left (the board) keeps its own scrollbars at the panel's bottom edge.
 */
export function Page({ children, className, fill }: { children?: ReactNode; className?: string; fill?: boolean }) {
  return (
    <>
      <style href="fx-layout" precedence="default">
        {css}
      </style>
      <div className={join('fx-page', className)} data-fill={fill || undefined}>
        {children}
      </div>
    </>
  )
}

export type PageHeaderProps = {
  title: ReactNode
  description?: ReactNode
  /** Where this page sits, e.g. a link back to the list a detail page came from. */
  back?: ReactNode
  action?: ReactNode
  meta?: ReactNode
}

export function PageHeader({ title, description, back, action, meta }: PageHeaderProps) {
  return (
    <>
      <style href="fx-layout" precedence="default">
        {css}
      </style>
      <header className="fx-pagehead">
        {back && <div className="fx-pagehead__back">{back}</div>}
        <div className="fx-pagehead__main">
          <div className="fx-pagehead__titles">
            <Heading as="h1" size="3">
              {title}
            </Heading>
            {description && (
              <Text as="p" size="2" tone="muted" className="fx-pagehead__desc">
                {description}
              </Text>
            )}
          </div>
          {action && <div className="fx-pagehead__action">{action}</div>}
        </div>
        {meta && <div className="fx-pagehead__meta">{meta}</div>}
      </header>
    </>
  )
}

const join = (...parts: (string | undefined)[]) => parts.filter(Boolean).join(' ')
const space = (s: Space) => (SPACE.includes(s) ? s : '3')

const css = /* css */ `
.fx-stack,
.fx-row,
.fx-grid {
  --gap: 12px;
  min-width: 0;
}
.fx-stack[data-gap="0"], .fx-row[data-gap="0"], .fx-grid[data-gap="0"] { --gap: 0; }
.fx-stack[data-gap="1"], .fx-row[data-gap="1"], .fx-grid[data-gap="1"] { --gap: 4px; }
.fx-stack[data-gap="2"], .fx-row[data-gap="2"], .fx-grid[data-gap="2"] { --gap: 8px; }
.fx-stack[data-gap="3"], .fx-row[data-gap="3"], .fx-grid[data-gap="3"] { --gap: 12px; }
.fx-stack[data-gap="4"], .fx-row[data-gap="4"], .fx-grid[data-gap="4"] { --gap: 16px; }
.fx-stack[data-gap="5"], .fx-row[data-gap="5"], .fx-grid[data-gap="5"] { --gap: 24px; }
.fx-stack[data-gap="6"], .fx-row[data-gap="6"], .fx-grid[data-gap="6"] { --gap: 36px; }

.fx-stack {
  display: flex;
  flex-direction: column;
  gap: var(--gap);
}
.fx-row {
  display: flex;
  flex-direction: row;
  gap: var(--gap);
}
.fx-row[data-wrap] {
  flex-wrap: wrap;
}
.fx-stack[data-align="start"], .fx-row[data-align="start"] { align-items: flex-start; }
.fx-stack[data-align="center"], .fx-row[data-align="center"] { align-items: center; }
.fx-stack[data-align="end"], .fx-row[data-align="end"] { align-items: flex-end; }
.fx-stack[data-align="baseline"], .fx-row[data-align="baseline"] { align-items: baseline; }
.fx-stack[data-align="stretch"], .fx-row[data-align="stretch"] { align-items: stretch; }
.fx-stack[data-justify="start"], .fx-row[data-justify="start"] { justify-content: flex-start; }
.fx-stack[data-justify="center"], .fx-row[data-justify="center"] { justify-content: center; }
.fx-stack[data-justify="end"], .fx-row[data-justify="end"] { justify-content: flex-end; }
.fx-stack[data-justify="between"], .fx-row[data-justify="between"] { justify-content: space-between; }

.fx-grid {
  display: grid;
  gap: var(--gap);
  grid-template-columns: repeat(auto-fit, minmax(min(var(--fx-grid-min, 240px), 100%), 1fr));
}

/* ---------- Type ---------- */

.fx-text {
  font-size: 14px;
  line-height: 1.5;
  margin: 0;
  min-width: 0;
}
.fx-text[data-size="1"] { font-size: 12.5px; line-height: 1.45; }
.fx-text[data-size="2"] { font-size: 14px; }
.fx-text[data-size="3"] { font-size: 15px; }
.fx-text[data-tone="muted"] { color: var(--aui-muted, #737373); }
.fx-text[data-tone="danger"] { color: var(--aui-danger, #c4320a); }
.fx-text[data-tone="success"] { color: var(--aui-success, #1e8a4f); }
.fx-text[data-tone="warning"] { color: var(--aui-warning, #b25e00); }
.fx-text[data-weight="medium"] { font-weight: 500; }
.fx-text[data-weight="strong"] { font-weight: 600; }
.fx-text[data-mono] {
  font-family: var(--aui-font-mono, ui-monospace, Menlo, Consolas, monospace);
  font-size: 0.92em;
  letter-spacing: 0;
}
.fx-text[data-break] {
  overflow-wrap: anywhere;
}

.fx-heading {
  margin: 0;
  color: var(--aui-ink, #171717);
  font-weight: 600;
  letter-spacing: -0.02em;
  line-height: 1.25;
  text-wrap: balance;
}
.fx-heading[data-size="1"] { font-size: 15px; letter-spacing: -0.01em; }
.fx-heading[data-size="2"] { font-size: 19px; }
.fx-heading[data-size="3"] { font-size: 25px; }

/* ---------- Page and section ---------- */

.fx-page {
  width: 100%;
  max-width: var(--fx-page-width, 1200px);
  margin: 0 auto;
  padding: 28px 20px 96px;
  display: flex;
  flex-direction: column;
  gap: 28px;
}
.fx-page[data-fill] {
  height: 100%;
  padding-bottom: 12px;
}
@media (max-width: 640px) {
  .fx-page {
    padding: 20px 16px 72px;
    gap: 22px;
  }
}

.fx-pagehead {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.fx-pagehead__back {
  font-size: 12.5px;
}
.fx-pagehead__main {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
  flex-wrap: wrap;
}
.fx-pagehead__titles {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}
.fx-pagehead__desc {
  max-width: 68ch;
}
.fx-pagehead__action {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.fx-pagehead__meta {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.fx-section {
  display: flex;
  flex-direction: column;
  gap: 12px;
  min-width: 0;
}
.fx-section__head {
  display: flex;
  align-items: flex-end;
  justify-content: space-between;
  gap: 12px;
  flex-wrap: wrap;
}
.fx-section__titles {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.fx-section__desc {
  max-width: 68ch;
}
.fx-section__action {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
`
