import { useMemo } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { MenuButton, SectionHead } from '@/components/ui/app-shell'
import { Badge } from '@/components/ui/badge'
import { Page, PageHeader, Row, Stack, Text } from '@/components/fx/layout'
import { Table } from '@/components/fx/table'
import { ProjectDot } from '@/components/fx/project'
import type { Overview, Session } from '../api'
import { Duration, Loaded, When, useApi } from '../ui'
import { COST_NOTE, money } from '../usage'

/// Every agent process the fleet has started, newest first — the fleet's session history,
/// filterable by project. This is the view that did not exist before: a run used to leave only
/// a transcript, so there was no way to ask "what has this project been doing".
export default function Sessions() {
  return (
    <Page>
      <PageHeader
        title="Sessions"
        description="One row per agent process the fleet has started. Open one to follow it live or continue it."
      />

      {/* The fleet's standing sessions have no task page worth the name; their sessions are
          what there is to see of them. */}
      <Row gap="3">
        <Text size="1" tone="muted">
          The fleet’s own:
        </Text>
        {[
          ['brain', 'Brain'],
          ['secretary', 'Secretary'],
          ['retro', 'Retrospective'],
        ].map(([id, name]) => (
          <Link key={id} to={`/tasks/${id}/session`} className="fx-link">
            {name}
          </Link>
        ))}
      </Row>

      <SessionList />
    </Page>
  )
}

/// The filter and the list, shared by this page and a project's own Sessions
/// (`/projects/<slug>/sessions`), where `project` is fixed by the page rather than chosen.
export function SessionList({ project: fixed }: { project?: string }) {
  const [params, setParams] = useSearchParams()
  const project = fixed ?? params.get('project') ?? ''
  const agent = params.get('agent') ?? ''

  const path = useMemo(() => {
    const u = new URLSearchParams()
    if (project) u.set('project', project)
    if (agent) u.set('agent', agent)
    u.set('limit', '200')
    return `/api/runs?${u}`
  }, [project, agent])

  const state = useApi<{ runs: Session[]; total: number }>(path, 15_000, { keepPrevious: true })
  const overview = useApi<Overview>(fixed === undefined ? '/api/overview' : null)

  const set = (key: string, value: string) => {
    const next = new URLSearchParams(params)
    if (value) next.set(key, value)
    else next.delete(key)
    setParams(next, { replace: true })
  }

  return (
    <Stack gap="3">
      <SectionHead
        title={fixed === undefined ? 'All sessions' : 'Sessions'}
        count={state.data?.total}
        right={
          fixed === undefined && (
            <MenuButton
              label="Project"
              value={project || 'all'}
              onChange={(v) => set('project', v === 'all' ? '' : v)}
              options={[
                { value: 'all', label: 'Every project' },
                ...(overview.data?.projects ?? []).map((p) => ({ value: p.slug, label: p.name || p.slug })),
                ...(project && !overview.data?.projects.some((p) => p.slug === project)
                  ? [{ value: project, label: project }]
                  : []),
              ]}
            />
          )
        }
      />

      {/* The list is fetched 200 at a time; the count above is everything that matches. */}
      {state.data && state.data.runs.length < state.data.total && (
        <Text size="1" tone="muted">
          Showing the first {state.data.runs.length} of {state.data.total}; narrow the filters to see the rest.
        </Text>
      )}
      <Loaded state={state}>
        {(d) => (
          <Table
            dense
            rows={d.runs}
            rowKey={(r) => r.run_id}
            loading={state.loading}
            empty="No session yet"
            columns={[
              {
                key: 'task',
                header: 'Task',
                width: '30%',
                cell: (r) => (
                  <Stack gap="1">
                    <Link to={`/tasks/${encodeURIComponent(r.task ?? '')}/session`} className="fx-link">
                      {r.task}
                    </Link>
                    <Text size="1" tone="muted" breakAll>
                      {r.title ?? ''}
                    </Text>
                  </Stack>
                ),
              },
              {
                key: 'agent',
                header: 'Agent',
                cell: (r) => (
                  <Stack gap="1">
                    <Text size="1">{r.agent ?? '—'}</Text>
                    {fixed === undefined && (
                      <Text size="1" tone="muted">
                        <ProjectDot slug={r.project} /> {r.project}
                      </Text>
                    )}
                  </Stack>
                ),
              },
              {
                key: 'node',
                header: 'Node',
                secondary: true,
                cell: (r) => (
                  <Text size="1" breakAll>
                    {r.node ?? '—'}
                  </Text>
                ),
              },
              { key: 'started', header: 'Started', cell: (r) => <When at={r.started_at} /> },
              {
                key: 'lasted',
                header: 'Lasted',
                align: 'end',
                cell: (r) => <Duration from={r.started_at} to={r.ended_at} />,
              },
              {
                key: 'cost',
                header: 'Cost',
                align: 'end',
                secondary: true,
                cell: (r) => (
                  <Text size="1" tone="muted" title={COST_NOTE}>
                    {r.cost_usd != null ? money(r.cost_usd) : '—'}
                  </Text>
                ),
              },
              {
                key: 'ended',
                header: 'Ended',
                cell: (r) =>
                  r.status === 'running' ? (
                    <Badge tone="success" live>
                      live
                    </Badge>
                  ) : (
                    <Stack gap="1">
                      {/* Either an exit code or why it was lost; both are worth seeing. */}
                      <Text size="1" tone={r.outcome?.startsWith('lost') ? 'warning' : 'muted'}>
                        {r.outcome ?? 'ended'}
                      </Text>
                      {r.session && (
                        <Text size="1" tone="muted" mono title="Claude's session id">
                          {r.session.slice(0, 8)}
                        </Text>
                      )}
                    </Stack>
                  ),
              },
            ]}
          />
        )}
      </Loaded>
    </Stack>
  )
}
