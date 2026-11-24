import { Badge } from '@/components/ui/badge'
import { Text } from '@/components/fx/layout'
import type { ClaudeUsage, TaskCost } from './api'

/*
 * What work costs and how much Claude usage a node has left.
 *
 * Costs are what Claude itself reports at the end of every turn: API prices, which on a
 * subscription measure how much was used rather than what was billed. So they are shown as a
 * measure, never as a bill.
 */

/** Dollars as a person reads them: `$0.42`, `$12.30`, `<$0.01`. */
export function money(usd: number): string {
  if (usd > 0 && usd < 0.01) return '<$0.01'
  return `$${usd.toFixed(2)}`
}

/** A token count as `950`, `12.4k` or `3.1M`. */
export function tokenCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`
  return `${n}`
}

/** A task's cost in one line, for its facts: `$0.75 · 1.5k tokens · 3 turns`. */
export function costLine(cost: TaskCost | null | undefined): string {
  if (!cost) return '—'
  return `${money(cost.usd)} · ${tokenCount(cost.tokens)} tokens · ${cost.turns} turn${cost.turns === 1 ? '' : 's'}`
}

/** What the cost figure is, for a `title` beside it. */
export const COST_NOTE = 'At API prices, as Claude reports it. On a subscription this measures use; it is not a bill.'

/** A share of a window, as `9%`. */
const pct = (v: number) => `${Math.round(v * 100)}%`

/** When a limit resets, as the owner reads it: a time today, or a day and time. */
function resetAt(at: string): string {
  const d = new Date(at)
  const sameDay = d.toDateString() === new Date().toDateString()
  return sameDay
    ? d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    : d.toLocaleString([], { weekday: 'short', hour: '2-digit', minute: '2-digit' })
}

/** A node's Claude usage at a glance: red while it is out (runs held), quiet otherwise. */
export function UsageBadge({ usage }: { usage: ClaudeUsage | null | undefined }) {
  if (usage?.limited_until) {
    return (
      <Badge tone="danger" dot>
        out until {resetAt(usage.limited_until)} · runs held
      </Badge>
    )
  }
  if (!usage || (usage.five_hour === null && usage.seven_day === null)) {
    return (
      <Text size="1" tone="muted">
        not reported
      </Text>
    )
  }
  const parts = [
    usage.five_hour !== null ? `5h ${pct(usage.five_hour)}` : null,
    usage.seven_day !== null ? `week ${pct(usage.seven_day)}` : null,
  ].filter(Boolean)
  const high = Math.max(usage.five_hour ?? 0, usage.seven_day ?? 0) >= 0.8
  return high ? (
    <Badge tone="warning" dot>
      {parts.join(' · ')}
    </Badge>
  ) : (
    <Text size="1" tone="muted">
      {parts.join(' · ')}
    </Text>
  )
}
