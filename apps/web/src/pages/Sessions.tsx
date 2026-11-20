import { useMemo } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Select } from '@/components/ui/select'
import { Page, PageHeader, Stack, Text } from '@/components/fx/layout'
import { Table } from '@/components/fx/table'
import type { Overview, Session } from '../api'
import { Duration, Loaded, When, useApi } from '../ui'

/// Every agent process the fleet has started, newest first — the fleet's session history,
/// filterable by project. This is the view that did not exist before: a run used to leave only
/// a transcript, so there was no way to ask "what has this project been doing".
export default function Sessions() {
  const [params, setParams] = useSearchParams()
  const project = params.get('project') ?? ''
  const agent = params.get('agent') ?? ''

  const path = useMemo(() => {
    const u = new URLSearchParams()
    if (project) u.set('project', project)
    if (agent) u.set('agent', agent)
    u.set('limit', '200')
    return `/api/runs?${u}`
  }, [project, agent])

  const state = useApi<{ runs: Session[]; total: number }>(path, 15_000)
  const overview = useApi<Overview>('/api/overview')

  const set = (key: string, value: string) => {
    const next = new URLSearchParams(params)
    if (value) next.set(key, value)
    else next.delete(key)
    setParams(next, { replace: true })
  }

  return (
    <Page>
      <PageHeader
        title="Sessions"
        description="One row per agent process the fleet has started."
        meta={
          state.data && (
            <Text size="1" tone="muted">
              {state.data.runs.length} of {state.data.total}
            </Text>
          )
        }
        action={
          <div style={{ minWidth: '12rem' }}>
            <Select
              size="sm"
              aria-label="Project"
              value={project || 'all'}
              onChange={(v) => set('project', v === 'all' ? '' : v)}
              options={[
                { value: 'all', label: 'Every project' },
                ...(overview.data?.projects ?? []).map((p) => ({ value: p.slug, label: p.name || p.slug })),
              ]}
            />
          </div>
        }
      />

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
                    <Link to={`/tasks/${r.task}`} className="fx-link">
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
                    <Text size="1" tone="muted">
                      {r.project}
                    </Text>
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
    </Page>
  )
}
