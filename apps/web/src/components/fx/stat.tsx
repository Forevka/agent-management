import type { ReactNode } from 'react'
import { useNavigate } from 'react-router-dom'
import { StatCard } from '@/components/ui/stat-card'

/*
 * Stat – one number, said once: affectUI's Stat Card (Pro), with what this app needs of it.
 *
 * A number worth looking into carries `to` and becomes a link to the page that explains it — a
 * figure that cannot be opened is a dead end. The card's own link is a plain anchor, so a plain
 * click is turned into the router's navigation here (no page reload), while a Ctrl+click or a
 * middle click still opens a new tab.
 */

export type StatProps = {
  label: string
  value: number | string
  /** One quiet line under the number. */
  hint?: ReactNode
  icon?: ReactNode
  /** Where the number can be looked into, as a router path. */
  to?: string
  /** Says the figure is still loading rather than genuinely zero. */
  loading?: boolean
}

export function Stat({ label, value, hint, icon, to, loading }: StatProps) {
  const navigate = useNavigate()
  return (
    <StatCard
      label={label}
      value={value}
      icon={icon}
      footer={hint}
      loading={loading}
      href={to}
      hitProps={
        to
          ? {
              onClick: (e) => {
                if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return
                e.preventDefault()
                navigate(to)
              },
            }
          : undefined
      }
    />
  )
}
