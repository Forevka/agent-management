import { useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import type { IconSvgElement } from '@hugeicons/react'
import {
  BrainCircuitIcon,
  GitBranchIcon,
  GithubIcon,
  GitlabIcon,
  GoogleIcon,
  McpServerIcon,
  SlackIcon,
  TelegramIcon,
  Ticket01Icon,
} from '@hugeicons/core-free-icons'
import { Icon, SearchField, Tabs } from '@/components/ui/app-shell'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { EmptyState } from '@/components/ui/empty-state'
import { TextField } from '@/components/ui/text-field'
import { Page, PageHeader, Row, Stack, Text } from '@/components/fx/layout'
import { Note } from '@/components/fx/note'
import { ProjectTag } from '@/components/fx/project'
import * as api from '../api'
import type { Integration, Integrations as IntegrationsT } from '../api'
import { Loaded, When, useApi } from '../ui'

/*
 * Integrations – every third-party access the fleet has, in the layout of affectUI's Integrations
 * screen: a bar with the categories and a search that stays at the top while the list scrolls,
 * then a section per category with the integrations in two columns. A row says what the service
 * is, how it stands now and what it is connected as; "Manage" opens its credential below it.
 *
 * Credentials are write-only: the page shows which keys are set, never a value, and a value typed
 * here is sent once and forgotten. Telegram and Hindsight are set in deploy/.env and only shown.
 */

const PATH = '/api/integrations'

const CATEGORIES: { tab: string; id: string; title: string }[] = [
  { tab: 'Messaging', id: 'messaging', title: 'Messaging' },
  { tab: 'Trackers', id: 'trackers', title: 'Trackers' },
  { tab: 'Code', id: 'code', title: 'Code' },
  { tab: 'Memory', id: 'memory', title: 'Memory' },
  { tab: 'MCP', id: 'mcp', title: 'MCP servers' },
]

const STATE: Record<string, { label: string; tone: 'neutral' | 'info' | 'success' | 'warning' | 'danger' }> = {
  connected: { label: 'connected', tone: 'success' },
  running: { label: 'running', tone: 'success' },
  ok: { label: 'working', tone: 'success' },
  idle: { label: 'not used yet', tone: 'neutral' },
  starting: { label: 'starting', tone: 'info' },
  unused: { label: 'no project uses it', tone: 'neutral' },
  off: { label: 'off', tone: 'neutral' },
  not_configured: { label: 'not set up', tone: 'neutral' },
  disconnected: { label: 'disconnected', tone: 'warning' },
  stopped: { label: 'stopped', tone: 'danger' },
  error: { label: 'failing', tone: 'danger' },
}

function iconOf(i: Integration): IconSvgElement {
  switch (i.kind) {
    case 'slack':
      return SlackIcon
    case 'gchat':
      return GoogleIcon
    case 'telegram':
      return TelegramIcon
    case 'jira':
      return Ticket01Icon
    case 'hindsight':
      return BrainCircuitIcon
    case 'mcp':
      return McpServerIcon
    case 'repo':
      return i.description.includes('github.com') ? GithubIcon : i.description.includes('gitlab') ? GitlabIcon : GitBranchIcon
    default:
      return GitBranchIcon
  }
}

export default function Integrations() {
  const [params, setParams] = useSearchParams()
  const tab = params.get('category') ?? 'All'
  const q = params.get('q') ?? ''
  const open = params.get('open')
  const state = useApi<IntegrationsT>(PATH, 20_000, { keepPrevious: true })

  const set = (key: string, value: string | null) => {
    const next = new URLSearchParams(params)
    if (value) next.set(key, value)
    else next.delete(key)
    setParams(next, { replace: true })
  }

  return (
    <Page>
      <style href="fx-integrations" precedence="default">
        {css}
      </style>
      <PageHeader
        title="Integrations"
        description="Every third-party access the fleet has: what each is connected as, whether it works right now, and its credential, which you set here and never see again."
      />
      {/* Stays at the top while the list scrolls away under it. */}
      <div className="fx-int-bar">
        <Tabs
          items={['All', ...CATEGORIES.map((c) => c.tab)]}
          value={tab}
          onChange={(v) => set('category', v === 'All' ? null : v)}
        />
        <SearchField placeholder="Search integrations" width={240} value={q} onChange={(v) => set('q', v || null)} />
      </div>
      <Loaded state={state}>
        {(d) => {
          const words = q.trim().toLowerCase()
          const keep = (i: Integration) =>
            !words ||
            `${i.name} ${i.description} ${i.folder ?? ''} ${(i.meta.projects ?? []).map((p) => `${p.slug} ${p.key}`).join(' ')}`
              .toLowerCase()
              .includes(words)
          const sections = CATEGORIES.filter((c) => tab === 'All' || c.tab === tab)
            .map((c) => ({ ...c, items: d.integrations.filter((i) => i.category === c.id && keep(i)) }))
            .filter((s) => s.items.length > 0)
          if (sections.length === 0) {
            return <EmptyState title="No integration matches" text="Try another name, or another category." />
          }
          return (
            <Stack gap="6">
              {sections.map((s) => (
                <section key={s.id} className="fx-int-section">
                  <h2 className="fx-int-section__title">{s.title}</h2>
                  <div className="fx-int-grid">
                    {s.items.map((i) => (
                      <IntegrationRow
                        key={i.id}
                        i={i}
                        open={open === i.id}
                        onToggle={() => set('open', open === i.id ? null : i.id)}
                        reload={() => void state.reload()}
                      />
                    ))}
                  </div>
                </section>
              ))}
            </Stack>
          )
        }}
      </Loaded>
    </Page>
  )
}

function IntegrationRow({
  i,
  open,
  onToggle,
  reload,
}: {
  i: Integration
  open: boolean
  onToggle: () => void
  reload: () => void
}) {
  const s = STATE[i.status.state] ?? { label: i.status.state, tone: 'neutral' as const }
  const link = i.meta.link
  return (
    <div className="fx-int-row" data-open={open || undefined}>
      <div className="fx-int-row__main">
        <span className="fx-int-logo" aria-hidden="true">
          <Icon icon={iconOf(i)} size={18} />
        </span>
        <span className="fx-int-row__text">
          <span className="fx-int-row__name">{i.name}</span>
          <span className="fx-int-row__desc" title={i.description}>
            {i.description}
          </span>
          <Row gap="2">
            <Badge tone={s.tone} dot>
              {s.label}
            </Badge>
            <Summary i={i} />
          </Row>
        </span>
        {i.editable ? (
          <Button size="sm" variant={i.configured ? 'secondary' : 'primary'} aria-expanded={open} onClick={onToggle}>
            {i.configured ? 'Manage' : 'Set up'}
          </Button>
        ) : link ? (
          <Link to={link} className="fx-link">
            <Text size="1">Open</Text>
          </Link>
        ) : null}
      </div>
      {open && i.editable && <Manage i={i} reload={reload} />}
    </div>
  )
}

/** One line of what it is connected as, or why it is not working. */
function Summary({ i }: { i: Integration }) {
  const m = i.meta
  if (i.status.detail && ['error', 'stopped', 'disconnected'].includes(i.status.state)) {
    return (
      <Text size="1" tone="danger" breakAll>
        {i.status.detail}
      </Text>
    )
  }
  const parts: React.ReactNode[] = []
  if (m.account) parts.push(`as ${m.account}`)
  if (m.mode) parts.push(m.mode)
  if (m.last_message) parts.push(<span key="lm">last message <When at={m.last_message} /></span>)
  if (m.last_read) parts.push(<span key="lr">read <When at={m.last_read} /></span>)
  if (typeof m.bots === 'number') parts.push(`${m.bots} agent bot${m.bots === 1 ? '' : 's'}`)
  if (typeof m.installed === 'number' && typeof m.nodes === 'number') parts.push(`installed on ${m.installed} of ${m.nodes} nodes`)
  if (parts.length === 0) return null
  return (
    <Text size="1" tone="muted">
      {parts.map((p, k) => (
        <span key={k}>
          {k > 0 ? ' · ' : ''}
          {p}
        </span>
      ))}
    </Text>
  )
}

function Manage({ i, reload }: { i: Integration; reload: () => void }) {
  const [test, setTest] = useState<{ ok: boolean; detail: string } | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const run = async (key: string, f: () => Promise<unknown>) => {
    setBusy(key)
    try {
      await f()
    } finally {
      setBusy(null)
    }
  }
  return (
    <div className="fx-int-manage">
      {i.meta.projects && i.meta.projects.length > 0 && (
        <Row gap="2">
          <Text size="1" tone="muted">
            Used by
          </Text>
          {i.meta.projects.map((p) => (
            <ProjectTag key={p.slug} slug={p.slug}>
              {p.name} · {p.key}
            </ProjectTag>
          ))}
        </Row>
      )}
      <Text size="1" tone="muted">
        Kept in the secret folder <code>{i.folder}</code>. A value is sent once and never shown again.
      </Text>
      <Stack gap="2">
        {i.keys.map((k) => (
          <KeyField key={k.key} i={i} k={k} busy={busy} run={run} reload={reload} />
        ))}
      </Stack>
      <Row gap="2">
        <Button
          size="sm"
          variant="secondary"
          loading={busy === 'test'}
          disabled={!i.configured}
          onClick={() =>
            void run('test', async () => {
              setTest(await api.act<{ ok: boolean; detail: string }>(PATH, 'test', { id: i.id }))
            })
          }
        >
          Test
        </Button>
        {!i.configured && (
          <Text size="1" tone="muted">
            Set the keys marked required first.
          </Text>
        )}
      </Row>
      {test && (
        <Note tone={test.ok ? 'success' : 'danger'} title={test.ok ? 'It works' : 'It does not work'}>
          {test.detail}
        </Note>
      )}
    </div>
  )
}

function KeyField({
  i,
  k,
  busy,
  run,
  reload,
}: {
  i: Integration
  k: Integration['keys'][number]
  busy: string | null
  run: (key: string, f: () => Promise<unknown>) => Promise<void>
  reload: () => void
}) {
  const [value, setValue] = useState('')
  const [error, setError] = useState<string | null>(null)
  const save = () =>
    run(`set:${k.key}`, async () => {
      try {
        await api.act(PATH, 'set', { id: i.id, key: k.key, value })
        setValue('')
        setError(null)
        reload()
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e))
      }
    })
  return (
    <form
      className="fx-int-key"
      onSubmit={(e) => {
        e.preventDefault()
        if (value.trim()) void save()
      }}
    >
      <div className="fx-int-key__field">
        <TextField
          size="sm"
          type="password"
          autoComplete="off"
          label={
            <Row gap="2">
              <code>{k.key}</code>
              <Badge tone={k.set ? 'success' : k.required ? 'warning' : 'neutral'}>
                {k.set ? 'set' : k.required ? 'required' : 'optional'}
              </Badge>
            </Row>
          }
          placeholder={k.set ? 'Type a new value to replace it' : 'Paste the value'}
          value={value}
          error={error ?? undefined}
          onChange={(e) => setValue(e.target.value)}
        />
      </div>
      <Button type="submit" size="sm" disabled={!value.trim()} loading={busy === `set:${k.key}`}>
        Save
      </Button>
      {k.set && (
        <Button
          type="button"
          size="sm"
          variant="ghost"
          loading={busy === `unset:${k.key}`}
          onClick={() => {
            if (!window.confirm(`Remove ${k.key}? ${i.name} stops working until it is set again.`)) return
            void run(`unset:${k.key}`, async () => {
              await api.act(PATH, 'unset', { id: i.id, key: k.key })
              reload()
            })
          }}
        >
          Remove
        </Button>
      )}
    </form>
  )
}

const css = /* css */ `
.fx-int-bar {
  position: sticky;
  top: 0;
  z-index: 5;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  margin: 0 -16px 8px;
  padding: 12px 16px;
  /* The shell's own panel colour, so the bar is the page and not a box on it. */
  background: var(--panel, var(--aui-surface, #ffffff));
}
/* A soft fade under the bar, so rows dissolve instead of being cut by a line. */
.fx-int-bar::after {
  content: "";
  position: absolute;
  left: 0;
  right: 0;
  top: 100%;
  height: 24px;
  background: linear-gradient(to bottom, var(--panel, var(--aui-surface, #ffffff)), transparent);
  pointer-events: none;
}
.fx-int-section__title {
  margin: 0 0 10px;
  font-size: 15px;
  line-height: 20px;
  font-weight: 500;
  color: var(--aui-ink, #171717);
}
.fx-int-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  column-gap: 32px;
}
@media (max-width: 720px) {
  .fx-int-grid { grid-template-columns: minmax(0, 1fr); }
}
.fx-int-row {
  border-bottom: 1px solid var(--aui-border, #e5e5e5);
  min-width: 0;
}
/* An open row takes the whole width, so its credential has room. */
.fx-int-row[data-open] {
  grid-column: 1 / -1;
}
.fx-int-row__main {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 12px 0;
  min-width: 0;
}
.fx-int-logo {
  display: grid;
  place-items: center;
  flex: none;
  width: 34px;
  height: 34px;
  border-radius: var(--aui-radius-sm, 8px);
  background: var(--aui-subtle, #f4f4f5);
  color: var(--aui-ink, #171717);
  box-shadow: inset 0 0 0 1px var(--aui-border, #e5e5e5);
}
.fx-int-row__text {
  display: flex;
  flex-direction: column;
  gap: 3px;
  flex: 1;
  min-width: 0;
}
.fx-int-row__name {
  font-size: 14px;
  font-weight: 500;
  color: var(--aui-ink, #171717);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.fx-int-row__desc {
  font-size: 12.5px;
  color: var(--aui-muted, #737373);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.fx-int-manage {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 4px 0 16px 46px;
}
.fx-int-key {
  display: flex;
  align-items: flex-end;
  flex-wrap: wrap;
  gap: 8px;
}
.fx-int-key__field {
  flex: 1;
  min-width: 14rem;
}
.fx-int-manage code {
  font-family: var(--aui-font-mono, ui-monospace, monospace);
  font-size: 12px;
}
`
