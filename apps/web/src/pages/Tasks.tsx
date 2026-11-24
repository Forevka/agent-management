import { useMemo } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { FilterButton, MenuButton, SearchField, SectionHead } from '@/components/ui/app-shell'
import { Badge } from '@/components/ui/badge'
import { Page, PageHeader, Stack, Text } from '@/components/fx/layout'
import { Table } from '@/components/fx/table'
import { ProjectTag } from '@/components/fx/project'
import type { Overview, Task } from '../api'
import { Loaded, StatusBadge, When, useApi } from '../ui'
import { TaskLink } from '../peek'

const STATUSES = ['queued', 'planning', 'running', 'waiting', 'done', 'stopped', 'failed', 'archived']

/// Every task the fleet has ever had, filtered. The filters live in the URL, so a view can be
/// bookmarked and the back button behaves.
export default function Tasks() {
  return (
    <Page>
      <PageHeader
        title="Tasks"
        description="Everything the fleet has worked on. Every word you search for must match somewhere; archived tasks are searched too. The filters are in the URL, so a view can be shared."
      />
      <TaskList />
    </Page>
  )
}

/// The filters and the list, shared by this page and a project's own Tasks (`/projects/<slug>/tasks`),
/// where `project` is fixed by the page rather than chosen.
export function TaskList({ project: fixed }: { project?: string }) {
  const [params, setParams] = useSearchParams()
  const project = fixed ?? params.get('project') ?? ''
  const status = params.get('status') ?? ''
  const q = params.get('q') ?? ''
  const builtin = params.get('builtin') === 'true'
  // Archived tasks are left out of a plain list (that is what archiving is for) and come back as
  // soon as the owner searches, filters on `archived`, or asks for them here.
  const archived = params.get('archived') === 'true'

  const path = useMemo(() => {
    const u = new URLSearchParams()
    if (project) u.set('project', project)
    if (status) u.set('status', status)
    if (q) u.set('q', q)
    if (builtin) u.set('builtin', 'true')
    if (archived) u.set('archived', 'true')
    u.set('limit', '200')
    return `/api/tasks?${u}`
  }, [project, status, q, builtin, archived])

  const state = useApi<{ tasks: Task[]; total: number }>(path, undefined, { keepPrevious: true })
  // Only for the project dropdown; the task list itself comes from the server filtered.
  const overview = useApi<Overview>(fixed === undefined ? '/api/overview' : null)

  const set = (key: string, value: string) => {
    const next = new URLSearchParams(params)
    if (value) next.set(key, value)
    else next.delete(key)
    setParams(next, { replace: true })
  }

  return (
    <Stack gap="3">
      {/* The Agent / Tasks screen's bar: a count, the search and the filters on one line. */}
      <SectionHead
        title={fixed === undefined ? 'All tasks' : 'Tasks'}
        count={state.data?.total}
        right={
          <>
            <SearchField
              placeholder="Search tasks, F-keys, links…"
              width={240}
              value={q}
              onChange={(v) => set('q', v)}
            />
            {fixed === undefined && (
              <MenuButton
                label="Project"
                value={project || 'all'}
                onChange={(v) => set('project', v === 'all' ? '' : v)}
                options={[
                  { value: 'all', label: 'Every project' },
                  ...(overview.data?.projects ?? []).map((p) => ({ value: p.slug, label: p.name || p.slug })),
                  // A project in the URL that the overview does not list (yet) still names itself.
                  ...(project && !overview.data?.projects.some((p) => p.slug === project)
                    ? [{ value: project, label: project }]
                    : []),
                ]}
              />
            )}
            <MenuButton
              label="Status"
              value={status || 'all'}
              onChange={(v) => set('status', v === 'all' ? '' : v)}
              options={[
                { value: 'all', label: 'Any status' },
                ...STATUSES.map((s) => ({ value: s, label: s })),
                ...(status && !STATUSES.includes(status) ? [{ value: status, label: status }] : []),
              ]}
            />
            {/* The brain, the secretary and the retrospective are standing sessions, not work;
                archived tasks are left out until asked for (or searched for). */}
            <FilterButton
              label="Include"
              groups={[
                {
                  id: 'include',
                  label: 'Include',
                  options: [
                    { id: 'builtin', label: "The fleet's own sessions" },
                    { id: 'archived', label: 'Archived tasks' },
                  ],
                },
              ]}
              value={{ include: [...(builtin ? ['builtin'] : []), ...(archived ? ['archived'] : [])] }}
              onChange={(v) => {
                const on = v.include ?? []
                const next = new URLSearchParams(params)
                for (const k of ['builtin', 'archived']) {
                  if (on.includes(k)) next.set(k, 'true')
                  else next.delete(k)
                }
                setParams(next, { replace: true })
              }}
            />
          </>
        }
      />
      {/* The list is fetched 200 at a time; the count above is everything that matches. */}
      {state.data && state.data.tasks.length < state.data.total && (
        <Text size="1" tone="muted">
          Showing the first {state.data.tasks.length} of {state.data.total}; narrow the filters to see the rest.
        </Text>
      )}
      <Loaded state={state}>
        {(d) => (
          <Table
            dense
            rows={d.tasks}
            rowKey={(t) => t.id}
            loading={state.loading}
            empty={q || status || (fixed === undefined && project) ? 'No task matches those filters' : 'No task yet'}
            columns={[
              {
                key: 'task',
                header: 'Task',
                width: '34%',
                cell: (t) => (
                  <Stack gap="1">
                    <TaskLink id={t.id}>{t.key && t.key !== t.id ? `${t.key} · ${t.id}` : t.id}</TaskLink>
                    <Text size="1" tone="muted" breakAll>
                      {t.title}
                    </Text>
                  </Stack>
                ),
              },
              // In a project's own list every row is that project.
              ...(fixed === undefined
                ? [
                    {
                      key: 'project',
                      header: 'Project',
                      secondary: true,
                      cell: (t: Task) => (
                        <Link to={`/projects/${t.project}`} className="fx-link" data-muted="">
                          <ProjectTag slug={t.project}>{t.project}</ProjectTag>
                        </Link>
                      ),
                    },
                  ]
                : []),
              {
                key: 'status',
                header: 'Status',
                // An archived task is still done; the list says which done ones were put away.
                cell: (t) => (
                  <StatusBadge status={t.status === 'done' && t.archived_at ? 'archived' : t.status} running={t.running} />
                ),
              },
              {
                key: 'agent',
                header: 'Agent',
                secondary: true,
                cell: (t) => <Text size="1">{t.agent ?? '—'}</Text>,
              },
              {
                key: 'sessions',
                header: 'Sessions',
                numeric: true,
                secondary: true,
                cell: (t) =>
                  t.sessions ? (
                    <Badge>{t.sessions}</Badge>
                  ) : (
                    <Text size="1" tone="muted">
                      —
                    </Text>
                  ),
              },
              { key: 'touched', header: 'Touched', align: 'end', cell: (t) => <When at={t.updated_at} /> },
            ]}
          />
        )}
      </Loaded>
    </Stack>
  )
}
