import { useMemo, useState } from 'react'
import { useSearchParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardFooter, CardHeader } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { Select } from '@/components/ui/select'
import { TextField } from '@/components/ui/text-field'
import { ScheduledRuns, type ScheduledTask } from '@/components/ui/scheduled-runs'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Table } from '@/components/fx/table'
import { ProjectDot, ProjectTag } from '@/components/fx/project'
import * as api from '../api'
import type { Roadmap as RoadmapT, RoadmapItem } from '../api'
import { ActionButton, ErrorNote, Loaded, When, useApi } from '../ui'
import { TaskLink } from '../peek'

/*
 * The roadmap: the owner's Jira sprints, worked through by the fleet (see `roadmap.rs`).
 *
 * The board is affectUI's Scheduled Runs: the sprint's days as columns, Claude's usage windows as
 * rows. It shows the approved plan only — what started stays where it started, what the owner
 * placed by hand stays there, and the rest is sorted in by the same rule the orchestrator starts
 * tickets by, so what the page shows is what will happen. Tickets still waiting for the owner are
 * listed above it, each with where it would go. Ticket titles are Jira's text, shown as text.
 */

const PATH = '/api/roadmap'

const STATE: Record<string, { label: string; tone: 'neutral' | 'info' | 'success' | 'warning' | 'danger' | 'purple' }> = {
  new: { label: 'waits for you', tone: 'warning' },
  approved: { label: 'planned', tone: 'info' },
  declined: { label: 'not now', tone: 'neutral' },
  skipped: { label: 'skipped', tone: 'neutral' },
  started: { label: 'started', tone: 'purple' },
  left: { label: 'left the sprint', tone: 'neutral' },
}

/** Where an item stands, as the owner reads it: Jira's "done" wins over a plan nobody will run. */
function stateOf(i: RoadmapItem) {
  if (i.task) {
    const s = i.task.status
    if (s === 'done') return { label: 'done', tone: 'success' as const }
    if (s === 'failed' || s === 'stopped') return { label: s, tone: 'danger' as const }
    return STATE.started
  }
  if (i.jira_done) return { label: 'done in Jira', tone: 'success' as const }
  return STATE[i.state] ?? { label: i.state, tone: 'neutral' as const }
}

/** The plan's first window for an item, as "Tue 24 Nov 12:00". */
function whenOf(d: RoadmapT, i: RoadmapItem): string | null {
  const p = i.at ?? i.planned[0]
  if (!p) return null
  const day = d.days[p.day]
  return day ? `${day.name} ${day.date} ${d.sessions[p.session] ?? ''}`.trim() : null
}

export default function Roadmap() {
  const [params, setParams] = useSearchParams()
  const sprint = params.get('sprint')
  const state = useApi<RoadmapT>(sprint ? `${PATH}?sprint=${encodeURIComponent(sprint)}` : PATH, 60_000)
  const reload = () => void state.reload()

  return (
    <Page>
      <PageHeader
        title="Roadmap"
        description="Your Jira sprints, worked through by the fleet. Every project with a Jira key has its sprint read every few minutes: the tickets assigned to you. Each day has Claude usage windows; a ticket starts on its own when its window opens, once you approve it, and the fleet moves it to In Progress and then to In Review with its merge request."
        action={
          <ActionButton op="sync" path={PATH} onDone={reload}>
            Read Jira now
          </ActionButton>
        }
      />
      <Loaded state={state}>
        {(d) => (
          <Body
            d={d}
            reload={reload}
            pick={(id) => {
              const next = new URLSearchParams(params)
              if (id === null) next.delete('sprint')
              else next.set('sprint', String(id))
              setParams(next)
            }}
          />
        )}
      </Loaded>
    </Page>
  )
}

function Body({ d, reload, pick }: { d: RoadmapT; reload: () => void; pick: (id: number | null) => void }) {
  const waiting = d.items.filter((i) => i.state === 'new' && !i.jira_done)
  const onBoard = d.items.filter((i) => i.on_board)
  const rest = d.items.filter((i) => !waiting.includes(i))
  return (
    <Stack gap="5">
      <Row gap="3" justify="between">
        <Row gap="2">
          <Select
            size="sm"
            aria-label="Sprint"
            value={String(d.sprint.id)}
            options={d.sprints.map((s) => ({
              value: String(s.id),
              label: `${s.name} · ${s.span}`,
              description: s.current ? 'current' : undefined,
            }))}
            onChange={(v) => pick(d.sprints.find((s) => String(s.id) === v)?.current ? null : Number(v))}
          />
          {!d.enabled && <Badge tone="warning">off (FLEET_ROADMAP)</Badge>}
        </Row>
        <SyncLine d={d} />
      </Row>

      {waiting.length > 0 && <Approvals d={d} items={waiting} reload={reload} />}

      <Plan d={d} items={onBoard} reload={reload} />

      <Section title="Every ticket" description="Everything this sprint has seen, in the order it waits in.">
        <Tickets d={d} items={[...waiting, ...rest]} reload={reload} />
      </Section>
    </Stack>
  )
}

function SyncLine({ d }: { d: RoadmapT }) {
  if (!d.sync) {
    return (
      <Text size="1" tone="muted">
        Not read from Jira yet
      </Text>
    )
  }
  const failed = Object.entries(d.sync.projects).filter(([, r]) => !r.ok)
  return (
    <Stack gap="1" align="end">
      <Text size="1" tone="muted">
        Read from Jira <When at={d.sync.at} />
      </Text>
      {failed.map(([p, r]) => (
        <Text key={p} size="1" tone="danger" breakAll>
          {p}: {r.error}
        </Text>
      ))}
    </Stack>
  )
}

function Approvals({ d, items, reload }: { d: RoadmapT; items: RoadmapItem[]; reload: () => void }) {
  return (
    <Card>
      <CardHeader
        title={`${items.length} ticket${items.length === 1 ? '' : 's'} wait for your approval`}
        description="Nothing starts before you approve it. Each shows where the plan would put it; the estimates are the orchestrator's, from reading the tickets."
      />
      <CardContent>
        <Stack gap="2">
          {items.map((i) => (
            <Row key={i.id} gap="3" justify="between">
              <Stack gap="1">
                <Row gap="2">
                  <JiraKey item={i} />
                  <ProjectTag slug={i.project}>{i.project}</ProjectTag>
                  <Text size="1" tone="muted">
                    {i.usage}% · {whenOf(d, i) ?? 'no room this sprint'}
                  </Text>
                </Row>
                <Text size="2" breakAll>
                  {i.title}
                </Text>
                {i.usage_note && (
                  <Text size="1" tone="muted" breakAll>
                    {i.usage_note}
                  </Text>
                )}
              </Stack>
              <Row gap="2">
                <ActionButton op="approve" path={PATH} body={{ ids: [i.id] }} size="sm" onDone={reload}>
                  Approve
                </ActionButton>
                <ActionButton op="decline" path={PATH} body={{ ids: [i.id] }} size="sm" variant="ghost" onDone={reload}>
                  Not now
                </ActionButton>
              </Row>
            </Row>
          ))}
        </Stack>
      </CardContent>
      <CardFooter>
        <ActionButton
          op="approve"
          path={PATH}
          body={{ ids: items.map((i) => i.id) }}
          variant="primary"
          onDone={reload}
        >
          Approve all {items.length}
        </ActionButton>
      </CardFooter>
    </Card>
  )
}

function Plan({ d, items, reload }: { d: RoadmapT; items: RoadmapItem[]; reload: () => void }) {
  const [error, setError] = useState<string | null>(null)
  // The board starts over whenever its tasks change identity, so they are rebuilt only when what
  // they say changes, not on every poll.
  const sig = JSON.stringify(items.map((i) => [i.id, i.plan_usage, i.at, i.state, i.task?.status, i.title]))
  const tasks = useMemo<ScheduledTask[]>(
    () =>
      items.map((i) => ({
        id: String(i.id),
        title: `${i.key} · ${i.title}`,
        usage: i.plan_usage,
        agent: i.project,
        logo: <ProjectDot slug={i.project} />,
        detail: [
          i.task ? `${i.task.key} · ${i.task.status}` : 'starts when its window opens',
          i.jira_status ? `Jira: ${i.jira_status}` : null,
          i.usage_note,
        ]
          .filter(Boolean)
          .join(' · '),
        ...(i.at ? { at: i.at } : {}),
      })),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [sig],
  )
  const place = async (id: string, to: { day: number; session: number }) => {
    setError(null)
    try {
      await api.act(PATH, 'place', { id: Number(id), day: to.day, session: to.session })
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
    reload()
  }
  const windows = `${Math.round(d.window_minutes / 60)}-hour windows`
  return (
    <Stack gap="2">
      {error && <ErrorNote>{error}</ErrorNote>}
      {items.length === 0 ? (
        <EmptyState
          title="Nothing planned yet"
          text="Approved tickets show here, in the window each starts in."
        />
      ) : (
        <ScheduledRuns
          key={d.sprint.id}
          tasks={tasks}
          days={d.days}
          sessions={d.sessions}
          now={{ day: d.now.day, session: d.now.session }}
          limit={d.limit}
          agents={[...new Set(items.map((i) => i.project))].map((p) => ({ name: p, logo: <ProjectDot slug={p} size={10} /> }))}
          title={`Sprint ${d.sprint.name}`}
          note={`${d.sprint.span} · ${windows}`}
          autoRun
          onPlace={(task, parts) => parts[0] && void place(task.id, parts[0])}
          onMove={(part, to) => {
            // A later piece of a split ticket was dragged: the ticket starts that many windows
            // earlier, so its first piece is what moves.
            const per = d.sessions.length
            const cell = Math.max(0, to.day * per + to.session - (part.part - 1))
            void place(part.task.id, { day: Math.floor(cell / per), session: cell % per })
          }}
        />
      )}
      <Text size="1" tone="muted">
        Drag a ticket to a window to start it there; it stays where you put it. Two tickets fit a
        window, up to its usage limit; a bigger one is split over the next windows.
      </Text>
    </Stack>
  )
}

function JiraKey({ item }: { item: RoadmapItem }) {
  return item.url ? (
    <a href={item.url} target="_blank" rel="noreferrer noopener" className="fx-link">
      <Text size="2" weight="medium" mono>
        {item.key}
      </Text>
    </a>
  ) : (
    <Text size="2" weight="medium" mono>
      {item.key}
    </Text>
  )
}

function Estimate({ item, reload }: { item: RoadmapItem; reload: () => void }) {
  // Mounted per estimate (see its key), so a newer estimate from the orchestrator replaces what
  // is shown, and leaving the field without typing saves nothing.
  const [value, setValue] = useState(String(item.usage))
  const [dirty, setDirty] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const save = async () => {
    const n = Number(value)
    if (!dirty || !Number.isFinite(n) || n === item.usage) return setValue(String(item.usage))
    try {
      setDirty(false)
      await api.act(PATH, 'estimate', { id: item.id, usage: Math.round(n) })
      setError(null)
      reload()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }
  if (item.state === 'started' || item.jira_done) return <Text size="2">{item.usage}%</Text>
  return (
    <TextField
      size="sm"
      type="number"
      min={5}
      max={1000}
      suffix="%"
      aria-label={`Estimate for ${item.key}`}
      value={value}
      error={error ?? undefined}
      title={item.usage_note ?? (item.estimated_by ? `Estimated by ${item.estimated_by}` : 'A guess by ticket type')}
      onChange={(e) => {
        setValue(e.target.value)
        setDirty(true)
      }}
      onBlur={() => void save()}
      onKeyDown={(e) => e.key === 'Enter' && void save()}
    />
  )
}

function Tickets({ d, items, reload }: { d: RoadmapT; items: RoadmapItem[]; reload: () => void }) {
  return (
    <Table
      dense
      rows={items}
      rowKey={(i) => String(i.id)}
      empty="No tickets in this sprint yet"
      columns={[
        {
          key: 'ticket',
          header: 'Ticket',
          width: '38%',
          cell: (i) => (
            <Stack gap="1">
              <Row gap="2">
                <JiraKey item={i} />
                <ProjectTag slug={i.project}>{i.project}</ProjectTag>
              </Row>
              <Text size="1" tone="muted" breakAll>
                {i.title}
              </Text>
            </Stack>
          ),
        },
        {
          key: 'state',
          header: 'State',
          cell: (i) => {
            const s = stateOf(i)
            return (
              <Stack gap="1">
                <Badge tone={s.tone} dot>
                  {s.label}
                </Badge>
                {i.jira_status && (
                  <Text size="1" tone="muted">
                    Jira: {i.jira_status}
                  </Text>
                )}
                {i.start_error && (
                  <Text size="1" tone="danger" breakAll>
                    Could not start: {i.start_error}
                  </Text>
                )}
              </Stack>
            )
          },
        },
        {
          key: 'when',
          header: 'Window',
          secondary: true,
          cell: (i) => (
            <Stack gap="1">
              <Text size="1">{whenOf(d, i) ?? '—'}</Text>
              {i.pinned && i.state !== 'started' && (
                <Text size="1" tone="muted">
                  placed by you
                </Text>
              )}
            </Stack>
          ),
        },
        {
          key: 'usage',
          header: 'Estimate',
          width: '12ch',
          cell: (i) => <Estimate key={`${i.id}:${i.usage}`} item={i} reload={reload} />,
        },
        {
          key: 'task',
          header: 'Task',
          secondary: true,
          cell: (i) => (i.task ? <TaskLink id={i.task.id}>{i.task.key}</TaskLink> : <Text tone="muted">—</Text>),
        },
        {
          key: 'act',
          header: '',
          align: 'end',
          cell: (i) => <Actions item={i} reload={reload} />,
        },
      ]}
    />
  )
}

function Actions({ item: i, reload }: { item: RoadmapItem; reload: () => void }) {
  if (i.state === 'started' || i.state === 'left' || i.jira_done) return null
  const body = { ids: [i.id] }
  return (
    <Row gap="1" justify="end">
      {i.state === 'declined' && (
        <ActionButton op="approve" path={PATH} body={body} size="sm" onDone={reload}>
          Approve
        </ActionButton>
      )}
      {i.pinned && (
        <ActionButton op="place" path={PATH} body={{ id: i.id }} size="sm" variant="ghost" onDone={reload}>
          Let the plan place it
        </ActionButton>
      )}
      {i.state === 'skipped' || i.state === 'declined' ? (
        <ActionButton op="restore" path={PATH} body={body} size="sm" variant="ghost" onDone={reload}>
          Restore
        </ActionButton>
      ) : (
        <ActionButton op="skip" path={PATH} body={body} size="sm" variant="ghost" onDone={reload}>
          Skip
        </ActionButton>
      )}
    </Row>
  )
}

