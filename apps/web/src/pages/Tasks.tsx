import { useMemo } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Select } from '@/components/ui/select'
import { TextField } from '@/components/ui/text-field'
import { Page, PageHeader, Row, Stack, Text } from '@/components/fx/layout'
import { Table } from '@/components/fx/table'
import { SearchIcon } from '@/components/fx/icons'
import type { Overview, Task } from '../api'
import { Loaded, StatusBadge, When, useApi } from '../ui'

const STATUSES = ['queued', 'planning', 'running', 'waiting', 'done', 'stopped', 'failed']

/// Every task the fleet has ever had, filtered. The filters live in the URL, so a view can be
/// bookmarked and the back button behaves.
export default function Tasks() {
  const [params, setParams] = useSearchParams()
  const project = params.get('project') ?? ''
  const status = params.get('status') ?? ''
  const q = params.get('q') ?? ''
  const builtin = params.get('builtin') === 'true'

  const path = useMemo(() => {
    const u = new URLSearchParams()
    if (project) u.set('project', project)
    if (status) u.set('status', status)
    if (q) u.set('q', q)
    if (builtin) u.set('builtin', 'true')
    u.set('limit', '200')
    return `/api/tasks?${u}`
  }, [project, status, q, builtin])

  const state = useApi<{ tasks: Task[]; total: number }>(path)
  // Only for the project dropdown; the task list itself comes from the server filtered.
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
        title="Tasks"
        description="Everything the fleet has worked on. The filters are in the URL, so a view can be shared."
        meta={
          state.data && (
            <Text size="1" tone="muted">
              {state.data.tasks.length} of {state.data.total}
            </Text>
          )
        }
      />

      <Row gap="2" align="end">
        <div style={{ minWidth: '15rem', flexGrow: 1 }}>
          <TextField
            placeholder="Search id or title"
            defaultValue={q}
            onChange={(e) => set('q', e.currentTarget.value)}
            prefix={<SearchIcon />}
            clearable
            onClear={() => set('q', '')}
            size="sm"
            aria-label="Search tasks"
          />
        </div>
        <div style={{ minWidth: '11rem' }}>
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
        <div style={{ minWidth: '10rem' }}>
          <Select
            size="sm"
            aria-label="Status"
            value={status || 'all'}
            onChange={(v) => set('status', v === 'all' ? '' : v)}
            options={[{ value: 'all', label: 'Any status' }, ...STATUSES.map((s) => ({ value: s, label: s }))]}
          />
        </div>
        <div style={{ minWidth: '13rem' }}>
          {/* The brain, the secretary and the retrospective are standing sessions, not work. */}
          <Select
            size="sm"
            aria-label="Include the fleet's own sessions"
            value={builtin ? 'true' : 'false'}
            onChange={(v) => set('builtin', v === 'true' ? 'true' : '')}
            options={[
              { value: 'false', label: 'Work only' },
              { value: 'true', label: "The fleet's own sessions too" },
            ]}
          />
        </div>
      </Row>

      <Loaded state={state}>
        {(d) => (
          <Table
            dense
            rows={d.tasks}
            rowKey={(t) => t.id}
            loading={state.loading}
            empty={q || status || project ? 'No task matches those filters' : 'No task yet'}
            columns={[
              {
                key: 'task',
                header: 'Task',
                width: '34%',
                cell: (t) => (
                  <Stack gap="1">
                    <Link to={`/tasks/${t.id}`} className="fx-link">
                      {t.id}
                    </Link>
                    <Text size="1" tone="muted" breakAll>
                      {t.title}
                    </Text>
                  </Stack>
                ),
              },
              {
                key: 'project',
                header: 'Project',
                secondary: true,
                cell: (t) => (
                  <Link to={`/projects/${t.project}`} className="fx-link" data-muted="">
                    {t.project}
                  </Link>
                ),
              },
              { key: 'status', header: 'Status', cell: (t) => <StatusBadge status={t.status} running={t.running} /> },
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
    </Page>
  )
}
