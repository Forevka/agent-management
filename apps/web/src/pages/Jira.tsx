import { useMemo } from 'react'
import { useSearchParams } from 'react-router-dom'
import { MenuButton, SearchField, SectionHead } from '@/components/ui/app-shell'
import { Avatar } from '@/components/ui/avatar'
import { Badge } from '@/components/ui/badge'
import { EmptyState } from '@/components/ui/empty-state'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { ProjectDot } from '@/components/fx/project'
import { Table } from '@/components/fx/table'
import type { JiraIssue, JiraPage } from '../api'
import { ActionButton, Loaded, When, useApi } from '../ui'
import { TaskLink } from '../peek'

/*
 * Every connected Jira in one place: each project's tickets in its active sprint, its next
 * sprints and its backlog, whoever holds them. The page reads the fleet's local copy
 * (`jira_sync.rs`), refreshed every few minutes, so it never waits on Jira and never holds a
 * credential. Tickets are grouped in the order they will be worked: the active sprint, then each
 * future sprint by its start, then the backlog. Everything a ticket says is Jira's text, shown as
 * text. Every filter is in the URL, so a view can be sent as a link.
 */

const PATH = '/api/jira'

const STATUS_TONE: Record<string, 'neutral' | 'info' | 'success'> = {
  new: 'neutral',
  indeterminate: 'info',
  done: 'success',
}

const day = (at: string | null) =>
  at ? new Date(at).toLocaleDateString('en-GB', { day: 'numeric', month: 'short' }) : null

/** A group's heading: the sprint's name and when it runs, or the backlog. */
function groupOf(i: JiraIssue): { key: string; title: string; note: string | null } {
  const s = i.sprint
  if (!s) return { key: 'backlog', title: 'Backlog', note: 'In no open sprint' }
  const from = day(s.starts_at)
  const to = day(s.ends_at)
  const note =
    s.state === 'active'
      ? `Active${to ? ` · ends ${to}` : ''}`
      : from
        ? `Starts ${from}${to ? ` · ends ${to}` : ''}`
        : 'Not started, no start date yet'
  return { key: `sprint:${s.id}`, title: s.name ?? `Sprint ${s.id}`, note }
}

export default function Jira() {
  const [params, setParams] = useSearchParams()
  const project = params.get('project') ?? ''
  const sprint = params.get('sprint') ?? 'all'
  const assignee = params.get('assignee') ?? 'any'
  const status = params.get('status') ?? 'open'
  const q = params.get('q') ?? ''

  const path = useMemo(() => {
    const u = new URLSearchParams()
    if (project) u.set('project', project)
    u.set('sprint', sprint)
    u.set('assignee', assignee)
    u.set('status', status)
    if (q) u.set('q', q)
    return `${PATH}?${u}`
  }, [project, sprint, assignee, status, q])
  const state = useApi<JiraPage>(path, 60_000, { keepPrevious: true })

  const set = (key: string, value: string, fallback: string) => {
    const next = new URLSearchParams(params)
    if (value && value !== fallback) next.set(key, value)
    else next.delete(key)
    // A sprint belongs to one project: changing the project drops it.
    if (key === 'project') next.delete('sprint')
    setParams(next, { replace: true })
  }
  const d = state.data

  return (
    <Page>
      <PageHeader
        title="Jira"
        description="Every ticket of every Jira you connected: the active sprint, the sprints after it and the backlog, whoever holds them. The fleet keeps a copy, read every few minutes, so this page never waits on Jira."
        action={
          <ActionButton op="refresh" path={PATH} onDone={() => void state.reload()}>
            Read Jira now
          </ActionButton>
        }
      />
      <Stack gap="4">
        {d && <SyncLine d={d} />}
        <SectionHead
          title="Tickets"
          count={d?.issues.length}
          right={
            <>
              <SearchField
                placeholder="Search keys, titles, people…"
                width={240}
                value={q}
                onChange={(v) => set('q', v, '')}
              />
              <MenuButton
                label="Project"
                value={project || 'all'}
                onChange={(v) => set('project', v === 'all' ? '' : v, '')}
                options={[
                  { value: 'all', label: 'Every project' },
                  ...(d?.projects ?? []).map((p) => ({ value: p.slug, label: `${p.name} (${p.key})` })),
                ]}
              />
              <MenuButton
                label="Sprint"
                value={sprint}
                onChange={(v) => set('sprint', v, 'all')}
                options={[
                  { value: 'all', label: 'Every sprint and the backlog' },
                  { value: 'active', label: 'Active sprint' },
                  { value: 'future', label: 'Future sprints' },
                  { value: 'backlog', label: 'Backlog' },
                  ...(d?.sprints ?? []).map((s) => ({
                    value: String(s.id),
                    label: `${s.name ?? s.id} (${s.count})`,
                  })),
                ]}
              />
              <MenuButton
                label="Assignee"
                value={assignee}
                onChange={(v) => set('assignee', v, 'any')}
                options={[
                  { value: 'any', label: 'Anyone' },
                  { value: 'me', label: 'Me' },
                  { value: 'none', label: 'Unassigned' },
                  ...(d?.assignees ?? [])
                    .filter((a) => a.account_id && !a.me)
                    .map((a) => ({ value: a.account_id!, label: `${a.name ?? a.account_id} (${a.count})` })),
                ]}
              />
              <MenuButton
                label="Status"
                value={status}
                onChange={(v) => set('status', v, 'open')}
                options={[
                  { value: 'open', label: 'Not done' },
                  { value: 'todo', label: 'To do' },
                  { value: 'progress', label: 'In progress' },
                  { value: 'done', label: 'Done' },
                  { value: 'any', label: 'Any status' },
                  ...(d?.statuses ?? [])
                    .filter((s) => s.name)
                    .map((s) => ({ value: s.name!, label: `${s.name} (${s.count})` })),
                ]}
              />
            </>
          }
        />
        {d?.more && (
          <Text size="1" tone="muted">
            Showing the first {d.issues.length}; narrow the filters to see the rest.
          </Text>
        )}
        <Loaded state={state}>{(data) => <Groups d={data} />}</Loaded>
      </Stack>
    </Page>
  )
}

function SyncLine({ d }: { d: JiraPage }) {
  const accounts = Object.entries(d.sync)
  if (accounts.length === 0) {
    return (
      <Text size="1" tone="muted">
        Not read yet. Give a project a Jira key on its settings page.
      </Text>
    )
  }
  return (
    <Stack gap="1">
      {accounts.map(([folder, a]) => (
        <Row key={folder} gap="2">
          <Badge tone={a.ok ? 'success' : 'danger'} dot>
            {a.site ?? folder}
          </Badge>
          <Text size="1" tone="muted">
            {a.account ? `as ${a.account} · ` : ''}read <When at={a.at} />
            {a.truncated ? ' · more tickets than the fleet keeps; the oldest were left out' : ''}
          </Text>
          {a.error && (
            <Text size="1" tone="danger" breakAll>
              {a.error}
            </Text>
          )}
        </Row>
      ))}
    </Stack>
  )
}

function Groups({ d }: { d: JiraPage }) {
  if (d.issues.length === 0) {
    return <EmptyState title="No ticket matches" text="Change the filters, or read Jira now." />
  }
  // The issues arrive in working order; keep it, one section per sprint and one for the backlog.
  const groups: { key: string; title: string; note: string | null; issues: JiraIssue[] }[] = []
  for (const i of d.issues) {
    const g = groupOf(i)
    const last = groups.find((x) => x.key === g.key)
    if (last) last.issues.push(i)
    else groups.push({ ...g, issues: [i] })
  }
  return (
    <Stack gap="5">
      {groups.map((g) => (
        <Section key={g.key} title={`${g.title} · ${g.issues.length}`} description={g.note ?? undefined}>
          <IssueTable issues={g.issues} />
        </Section>
      ))}
    </Stack>
  )
}

function IssueTable({ issues }: { issues: JiraIssue[] }) {
  return (
    <Table
      dense
      rows={issues}
      rowKey={(i) => `${i.project}:${i.key}`}
      columns={[
        {
          key: 'ticket',
          header: 'Ticket',
          width: '46%',
          cell: (i) => (
            <Stack gap="1">
              <Row gap="2" wrap={false}>
                <ProjectDot slug={i.project} />
                <a href={i.url} target="_blank" rel="noopener noreferrer" className="fx-link">
                  <Text size="2" weight="medium" mono>
                    {i.key}
                  </Text>
                </a>
                {i.type && (
                  <Text size="1" tone="muted">
                    {i.type}
                    {i.priority ? ` · ${i.priority}` : ''}
                  </Text>
                )}
              </Row>
              <Text size="2" breakAll>
                {i.summary}
              </Text>
            </Stack>
          ),
        },
        {
          key: 'status',
          header: 'Status',
          cell: (i) => (
            <Badge tone={STATUS_TONE[i.status_category ?? ''] ?? 'neutral'} dot>
              {i.status ?? '—'}
            </Badge>
          ),
        },
        {
          key: 'assignee',
          header: 'Assignee',
          secondary: true,
          cell: (i) =>
            i.assignee ? (
              <Row gap="2" wrap={false}>
                <Avatar name={i.assignee.name ?? i.assignee.account_id} seed={i.assignee.account_id} size={20} />
                <Text size="1">{i.me ? 'You' : (i.assignee.name ?? 'someone')}</Text>
              </Row>
            ) : (
              <Text size="1" tone="muted">
                Unassigned
              </Text>
            ),
        },
        {
          key: 'roadmap',
          header: 'Fleet',
          cell: (i) =>
            i.roadmap?.task ? (
              <TaskLink id={i.roadmap.task.id}>{i.roadmap.task.key}</TaskLink>
            ) : i.roadmap ? (
              <Badge tone="info">on the roadmap</Badge>
            ) : (
              <Text size="1" tone="muted">
                —
              </Text>
            ),
        },
        { key: 'updated', header: 'Updated', align: 'end', secondary: true, cell: (i) => <When at={i.updated} /> },
      ]}
    />
  )
}

