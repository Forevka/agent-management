import type { ReactNode } from 'react'
import { EmptyState } from '@/components/ui/empty-state'
import { Skeleton } from '@/components/ui/skeleton'

/*
 * Table – a card with rows in it.
 *
 * affectUI's own Data Table is a Pro item, so this is ours, written to the same contract: one
 * file, its styles injected below, variants as `data-*`, every colour from an `--aui-*` token,
 * and dark mode through `:where(.dark, [data-theme="dark"])`. It is declarative on purpose —
 * the pages describe their columns and hand over rows, so no page writes table markup, and a
 * change to how a table reads happens here, once.
 *
 * It does not sort. Every list in this app arrives ordered by the API (and filtered by URL), so
 * a column header that re-sorts the page it is on would be a second, disagreeing source of
 * truth. If sorting is wanted, it belongs in the query.
 */

export type Column<T> = {
  /** Identifies the column; also the React key for its cells. */
  key: string
  header: ReactNode
  cell: (row: T) => ReactNode
  /** A width hint, e.g. "22%" or "12ch". Columns with none share what is left. */
  width?: string
  align?: 'start' | 'center' | 'end'
  /** Numbers line up under each other and right-align. */
  numeric?: boolean
  /** Dropped on a narrow screen rather than squeezing the columns that matter. */
  secondary?: boolean
}

export type TableProps<T> = {
  columns: Column<T>[]
  rows: T[]
  rowKey: (row: T, index: number) => string
  /** Shown in place of the rows when there are none. A string becomes the title. */
  empty?: ReactNode
  /** Skeleton rows while the first load is in flight. */
  loading?: boolean
  /** Tighter rows, for the long lists (sessions, tasks). */
  dense?: boolean
  /** Scrolls inside the card past this height, with the header staying put. */
  maxHeight?: number | string
  /** A row the page wants to point at, e.g. the one the owner just acted on. */
  highlight?: (row: T) => boolean
  caption?: ReactNode
  className?: string
}

export function Table<T>({
  columns,
  rows,
  rowKey,
  empty = 'Nothing here yet',
  loading,
  dense,
  maxHeight,
  highlight,
  caption,
  className,
}: TableProps<T>) {
  const body = loading ? (
    <tbody>
      {Array.from({ length: 4 }, (_, i) => (
        <tr key={i} className="fx-table__row" aria-hidden="true">
          {columns.map((c) => (
            <td key={c.key} className="fx-table__cell" data-secondary={c.secondary || undefined}>
              <Skeleton width={i % 2 ? '55%' : '75%'} height={12} />
            </td>
          ))}
        </tr>
      ))}
    </tbody>
  ) : (
    <tbody>
      {rows.map((row, i) => (
        <tr
          key={rowKey(row, i)}
          className="fx-table__row"
          data-highlight={highlight?.(row) || undefined}
        >
          {columns.map((c) => (
            <td
              key={c.key}
              className="fx-table__cell"
              data-align={c.align}
              data-numeric={c.numeric || undefined}
              data-secondary={c.secondary || undefined}
            >
              {c.cell(row)}
            </td>
          ))}
        </tr>
      ))}
    </tbody>
  )

  return (
    <>
      <style href="fx-table" precedence="default">
        {css}
      </style>
      <div className={['fx-table', className].filter(Boolean).join(' ')} data-dense={dense || undefined}>
        {!loading && rows.length === 0 ? (
          <div className="fx-table__empty">
            {typeof empty === 'string' ? <EmptyState title={empty} bare /> : empty}
          </div>
        ) : (
          <div
            className="fx-table__scroll"
            style={maxHeight ? { maxHeight: typeof maxHeight === 'number' ? `${maxHeight}px` : maxHeight } : undefined}
            data-capped={maxHeight ? '' : undefined}
          >
            <table className="fx-table__table">
              {caption && <caption className="fx-table__caption">{caption}</caption>}
              <thead className="fx-table__head">
                <tr>
                  {columns.map((c) => (
                    <th
                      key={c.key}
                      scope="col"
                      className="fx-table__th"
                      style={c.width ? { width: c.width } : undefined}
                      data-align={c.align}
                      data-numeric={c.numeric || undefined}
                      data-secondary={c.secondary || undefined}
                    >
                      {c.header}
                    </th>
                  ))}
                </tr>
              </thead>
              {body}
            </table>
          </div>
        )}
      </div>
    </>
  )
}

/** Two columns of label and value: what a detail page says about one thing. */
export function Facts({ items, className }: { items: [ReactNode, ReactNode][]; className?: string }) {
  return (
    <>
      <style href="fx-table" precedence="default">
        {css}
      </style>
      <dl className={['fx-facts', className].filter(Boolean).join(' ')}>
        {items.map(([label, value], i) => (
          <div className="fx-facts__item" key={i}>
            <dt className="fx-facts__label">{label}</dt>
            <dd className="fx-facts__value">{value}</dd>
          </div>
        ))}
      </dl>
    </>
  )
}

const css = /* css */ `
.fx-table {
  --fx-tbl-surface: var(--aui-surface, #ffffff);
  --fx-tbl-line: var(--aui-border, #e5e5e5);
  --fx-tbl-head: var(--aui-subtle, #f0f0f0);
  --fx-tbl-ink: var(--aui-ink, #171717);
  --fx-tbl-muted: var(--aui-muted, #737373);
  --fx-tbl-hover: color-mix(in srgb, var(--aui-subtle, #f0f0f0) 65%, var(--aui-surface, #fff));
  --fx-tbl-mark: color-mix(in srgb, var(--aui-info-soft, #eaf1ff) 80%, var(--aui-surface, #fff));
  --fx-tbl-radius: var(--aui-radius-lg, 16px);
  --fx-tbl-py: 11px;
  --fx-tbl-px: 14px;

  background: var(--fx-tbl-surface);
  border: 1px solid var(--fx-tbl-line);
  border-radius: var(--fx-tbl-radius);
  box-shadow: var(--aui-shadow-card, 0 1px 2px rgba(0, 0, 0, 0.06));
  overflow: clip;
  min-width: 0;
}
.fx-table[data-dense] {
  --fx-tbl-py: 8px;
}

/* The card clips, so the scroller inside it needs no radius of its own. */
.fx-table__scroll {
  overflow: auto;
  overscroll-behavior-x: contain;
}

.fx-table__table {
  width: 100%;
  border-collapse: separate;
  border-spacing: 0;
  font-size: 13.5px;
  color: var(--fx-tbl-ink);
}

.fx-table__caption {
  caption-side: bottom;
  padding: 10px var(--fx-tbl-px);
  color: var(--fx-tbl-muted);
  font-size: 12.5px;
  text-align: left;
  border-top: 1px solid var(--fx-tbl-line);
}

.fx-table__head th {
  position: sticky;
  top: 0;
  z-index: 1;
  background: var(--fx-tbl-head);
  color: var(--fx-tbl-muted);
  font-size: 12px;
  font-weight: 500;
  letter-spacing: 0.01em;
  text-align: left;
  white-space: nowrap;
  padding: 9px var(--fx-tbl-px);
  border-bottom: 1px solid var(--fx-tbl-line);
}
/* Rounded top corners would show through the sticky header's background otherwise. */
.fx-table__head th:first-child { border-top-left-radius: calc(var(--fx-tbl-radius) - 1px); }
.fx-table__head th:last-child { border-top-right-radius: calc(var(--fx-tbl-radius) - 1px); }

.fx-table__row {
  transition: background-color 120ms ease;
}
.fx-table__row:hover {
  background: var(--fx-tbl-hover);
}
.fx-table__row[data-highlight] {
  background: var(--fx-tbl-mark);
}

.fx-table__cell {
  padding: var(--fx-tbl-py) var(--fx-tbl-px);
  border-top: 1px solid var(--fx-tbl-line);
  vertical-align: top;
  min-width: 0;
}
.fx-table__row:first-child .fx-table__cell {
  border-top: 0;
}

.fx-table__th[data-align="center"], .fx-table__cell[data-align="center"] { text-align: center; }
.fx-table__th[data-align="end"], .fx-table__cell[data-align="end"] { text-align: right; }
.fx-table__th[data-numeric], .fx-table__cell[data-numeric] {
  text-align: right;
  font-variant-numeric: tabular-nums;
}

.fx-table__empty {
  padding: 8px;
}

/* A phone keeps the columns that carry the point and drops the rest. */
@media (max-width: 720px) {
  .fx-table__th[data-secondary],
  .fx-table__cell[data-secondary] {
    display: none;
  }
  .fx-table {
    --fx-tbl-px: 11px;
  }
}

/* ---------- Facts ---------- */

.fx-facts {
  --fx-facts-line: var(--aui-border, #e5e5e5);
  margin: 0;
  display: grid;
  gap: 0;
}
.fx-facts__item {
  display: grid;
  grid-template-columns: minmax(120px, 22%) 1fr;
  gap: 16px;
  padding: 9px 0;
  border-top: 1px solid var(--fx-facts-line);
}
.fx-facts__item:first-child {
  border-top: 0;
  padding-top: 0;
}
.fx-facts__label {
  margin: 0;
  color: var(--aui-muted, #737373);
  font-size: 12.5px;
}
.fx-facts__value {
  margin: 0;
  font-size: 13.5px;
  min-width: 0;
  overflow-wrap: anywhere;
}
@media (max-width: 560px) {
  .fx-facts__item {
    grid-template-columns: 1fr;
    gap: 2px;
  }
}
`
