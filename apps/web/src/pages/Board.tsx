import { useEffect, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { ViewToggle } from '@/components/ui/app-shell'
import { Board, BoardCard, BoardColumn, BoardDot } from '@/components/fx/board'
import type { BoardTone } from '@/components/fx/board'
import { Page, PageHeader, Row, Stack, Text } from '@/components/fx/layout'
import { Table } from '@/components/fx/table'
import {
  MilestoneIcon,
  PaperclipIcon,
  SessionsIcon,
  SparkIcon,
  WarningIcon,
} from '@/components/fx/icons'
import type { BoardTask } from '../api'
import { Loaded, StatusBadge, When, useApi } from '../ui'
import { TaskLink, useTaskPeek } from '../peek'

/*
 * The fleet's work, as a board.
 *
 * One column per state a task can really be in, so the board is a picture of the fleet rather
 * than a plan someone has to keep up to date. The first column is the only derived one: whatever
 * wants the owner — an open question, a plan to approve, or a task that failed and will not move
 * again on its own. That is the column worth looking at first, so it comes first.
 */

/** A column, and which tasks fall into it. A task lands in the first one that takes it. */
const COLUMNS: {
  key: string
  title: string
  tone: BoardTone
  /** Shown even when empty: the three a reader expects to find every time. */
  always?: boolean
  empty: string
  takes: (t: BoardTask) => boolean
}[] = [
  {
    key: 'needs-you',
    title: 'Needs you',
    tone: 'warning',
    always: true,
    empty: 'Nothing is waiting on you.',
    // A failure belongs here too: nobody else will pick it up.
    takes: (t) => t.needs_you !== null || t.status === 'failed',
  },
  { key: 'queued', title: 'Queued', tone: 'default', empty: 'Nothing waiting for an agent.', takes: (t) => t.status === 'queued' },
  { key: 'planning', title: 'Planning', tone: 'info', empty: 'No plan being written.', takes: (t) => t.status === 'planning' },
  {
    key: 'running',
    title: 'Running',
    tone: 'success',
    always: true,
    empty: 'No agent is working.',
    takes: (t) => t.status === 'running',
  },
  { key: 'waiting', title: 'Waiting', tone: 'default', empty: 'Nothing on a timer.', takes: (t) => t.status === 'waiting' },
  { key: 'done', title: 'Done', tone: 'success', always: true, empty: 'Nothing finished today.', takes: (t) => t.status === 'done' },
  { key: 'stopped', title: 'Stopped', tone: 'default', empty: 'Nothing stopped.', takes: (t) => t.status === 'stopped' },
]

/** Which columns the owner has folded away. Kept per browser, not per fleet: it is a view. */
const FOLDED_KEY = 'fleet.board.folded'

function useFolded() {
  const [folded, setFolded] = useState<string[]>(() => {
    // Storage can be unavailable (private window, blocked site data) and must never stop the
    // board rendering, so a failed read is simply "nothing folded".
    try {
      const raw = localStorage.getItem(FOLDED_KEY)
      return raw ? (JSON.parse(raw) as string[]) : []
    } catch {
      return []
    }
  })
  const toggle = (key: string) =>
    setFolded((prev) => {
      const next = prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key]
      try {
        localStorage.setItem(FOLDED_KEY, JSON.stringify(next))
      } catch {
        // A preference that cannot be saved is still worth having for this visit.
      }
      return next
    })
  return [folded, toggle] as const
}

export default function BoardPage() {
  const [params, setParams] = useSearchParams()
  const [folded, toggleFolded] = useFolded()
  const view = params.get('view') === 'list' ? 'list' : 'board'
  // The fleet moves while the page is open; the durations below tick on their own.
  const state = useApi<{ tasks: BoardTask[] }>('/api/board', 10_000)
  const peek = useTaskPeek()
  return (
    <Page fill={view === 'board'}>
      <PageHeader
        title="Board"
        description="Every task the fleet has in hand, in the state it is actually in."
        action={
          // The Kanban screen's grid/list toggle. The view is part of the URL, like every other
          // filter in this app, so a link to the board is a link to what the sender was looking at.
          <ViewToggle
            value={view === 'list' ? 'list' : 'grid'}
            onChange={(v) => {
              const next = new URLSearchParams(params)
              if (v === 'list') next.set('view', 'list')
              else next.delete('view')
              setParams(next, { replace: true })
            }}
          />
        }
      />
      <Loaded state={state}>
        {(d) => {
          const columns = COLUMNS.map((c) => ({ ...c, tasks: [] as BoardTask[] }))
          for (const t of d.tasks) {
            const col = columns.find((c) => c.takes(t))
            if (col) col.tasks.push(t)
          }
          if (view === 'list') return <BoardList tasks={d.tasks} />
          const shown = columns.filter((c) => c.always || c.tasks.length > 0)
          return (
            <Board>
              {shown.map((c) => (
                <BoardColumn
                  key={c.key}
                  title={c.title}
                  count={c.tasks.length}
                  tone={c.tone}
                  icon={<SparkIcon />}
                  empty={c.empty}
                  collapsed={folded.includes(c.key)}
                  onToggle={() => toggleFolded(c.key)}
                >
                  {c.tasks.map((t) => (
                    <BoardCard
                      key={t.id}
                      to={peek.to(t.id)}
                      onClick={peek.onClick(t.id)}
                      title={t.title || t.id}
                      badge={t.key}
                      mark={<SparkIcon />}
                      text={t.brief}
                      tone={cardTone(t)}
                      state={<CardState task={t} />}
                      links={(t.links ?? []).map((l) => ({
                        key: `${l.kind}:${l.id}`,
                        label: l.kind === 'url' ? host(l.ref) : l.ref,
                        title: l.url ?? l.ref,
                        url: l.url,
                      }))}
                      facts={[
                        {
                          icon: <SessionsIcon />,
                          value: t.sessions ?? 0,
                          title: `${t.sessions ?? 0} session(s) so far`,
                        },
                        {
                          icon: <MilestoneIcon />,
                          value: t.milestones > 0 ? `${t.milestones_done}/${t.milestones}` : '—',
                          title:
                            t.milestones > 0
                              ? `${t.milestones_done} of ${t.milestones} steps done`
                              : 'No checklist yet',
                        },
                        // What it produced, only once there is something: a count, because the
                        // titles belong on the task's page.
                        ...((t.attachments ?? []).length > 0
                          ? [
                              {
                                icon: <PaperclipIcon />,
                                value: (t.attachments ?? []).length,
                                title: (t.attachments ?? []).map((a) => a.title).join('\n'),
                              },
                            ]
                          : []),
                      ]}
                    />
                  ))}
                </BoardColumn>
              ))}
            </Board>
          )
        }}
      </Loaded>
    </Page>
  )
}

/** The colour a card carries: what it means, never decoration. */
function cardTone(t: BoardTask): BoardTone {
  if (t.status === 'failed') return 'danger'
  if (t.needs_you) return t.needs_you.blocking ? 'danger' : 'warning'
  if (t.status === 'running') return 'success'
  if (t.status === 'done') return 'success'
  if (t.status === 'planning') return 'info'
  return 'default'
}

/**
 * The one line that says where a task stands.
 *
 * Anything still costing time counts up, because the number is the point: how long an agent has
 * been held up, or how long it has been working. Anything settled says how long ago instead.
 */
function CardState({ task: t }: { task: BoardTask }) {
  if (t.needs_you) {
    return (
      <>
        <WarningIcon />
        Waiting for you · <Elapsed since={t.needs_you.since} />
      </>
    )
  }
  if (t.status === 'failed') return <><BoardDot tone="danger" />Failed · <When at={t.since} /></>
  if (t.status === 'stopped') return <><BoardDot />Stopped · <When at={t.since} /></>
  if (t.status === 'done') return <><BoardDot tone="success" />Done · <When at={t.since} /></>
  if (t.running) return <><BoardDot tone="success" spin /><Elapsed since={t.since} /></>
  // Nothing is being spent on the work right now, but for three different reasons: no agent is
  // free, the agent is asleep until a wakeup fires, or its session is simply between turns.
  const idle = t.status === 'queued' ? 'Queued' : t.status === 'waiting' ? 'Waiting' : 'Idle'
  return <><BoardDot />{idle} · <Elapsed since={t.since} /></>
}

/** A URL shown as its host, since a whole URL will not fit a chip. */
function host(url: string): string {
  try {
    return new URL(url).host.replace(/^www\./, '')
  } catch {
    // Not a URL the browser can parse; show the start of it rather than nothing.
    return url.slice(0, 24)
  }
}

/** A duration that counts up while the page is open, as `6m 17s`. */
function Elapsed({ since }: { since: string }) {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [])
  const secs = Math.max(0, Math.round((now - new Date(since).getTime()) / 1000))
  const h = Math.floor(secs / 3600)
  const m = Math.floor((secs % 3600) / 60)
  const s = secs % 60
  return <span>{h > 0 ? `${h}h ${m}m` : m > 0 ? `${m}m ${s}s` : `${s}s`}</span>
}

/** The same work as rows, for when there is more of it than a board reads well. */
function BoardList({ tasks }: { tasks: BoardTask[] }) {
  return (
    <Table
      rows={tasks}
      rowKey={(t) => t.id}
      empty="The fleet has nothing in hand"
      columns={[
        {
          key: 'task',
          header: 'Task',
          width: '42%',
          cell: (t) => (
            <Stack gap="1">
              <TaskLink id={t.id}>{t.title || t.id}</TaskLink>
              <Row gap="2">
                <Text size="1" tone="muted" mono>
                  {t.key} · {t.id}
                </Text>
                {/* Straight to the task page's edit form, which is part of its URL. */}
                <Link to={`/tasks/${t.id}?edit=1`} className="fx-link" data-muted="">
                  <Text size="1">Edit</Text>
                </Link>
              </Row>
            </Stack>
          ),
        },
        {
          key: 'state',
          header: 'State',
          cell: (t) =>
            t.needs_you ? (
              <Text size="1" tone={t.needs_you.blocking ? 'danger' : 'warning'}>
                needs you · {t.needs_you.kind}
              </Text>
            ) : (
              <StatusBadge status={t.status} running={t.running} />
            ),
        },
        { key: 'agent', header: 'Agent', secondary: true, cell: (t) => <Text size="1">{t.agent ?? '—'}</Text> },
        {
          key: 'sessions',
          header: 'Sessions',
          align: 'end',
          numeric: true,
          cell: (t) => <Text size="1">{t.sessions ?? 0}</Text>,
        },
        {
          key: 'steps',
          header: 'Steps',
          align: 'end',
          numeric: true,
          cell: (t) => <Text size="1">{t.milestones > 0 ? `${t.milestones_done}/${t.milestones}` : '—'}</Text>,
        },
        { key: 'touched', header: 'Touched', align: 'end', cell: (t) => <When at={t.updated_at} /> },
      ]}
    />
  )
}
