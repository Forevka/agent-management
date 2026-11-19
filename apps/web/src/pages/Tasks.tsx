import { useMemo } from 'react'
import { Badge, Box, Flex, Heading, Select, Table, Text, TextField } from '@radix-ui/themes'
import { MagnifyingGlassIcon } from '@radix-ui/react-icons'
import { Link, useSearchParams } from 'react-router-dom'
import type { Overview, Task } from '../api'
import { Empty, Loaded, StatusBadge, When, useApi } from '../ui'

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
    <>
      <Heading size="6" mt="5" mb="3">
        Tasks
      </Heading>

      <Flex gap="2" wrap="wrap" align="center" mb="3">
        <Box style={{ minWidth: '14rem', flexGrow: 1 }}>
          <TextField.Root
            placeholder="Search id or title"
            defaultValue={q}
            onChange={(e) => set('q', e.currentTarget.value)}
          >
            <TextField.Slot>
              <MagnifyingGlassIcon />
            </TextField.Slot>
          </TextField.Root>
        </Box>
        <Select.Root value={project || 'all'} onValueChange={(v) => set('project', v === 'all' ? '' : v)}>
          <Select.Trigger placeholder="Project" />
          <Select.Content>
            <Select.Item value="all">Every project</Select.Item>
            {(overview.data?.projects ?? []).map((p) => (
              <Select.Item key={p.slug} value={p.slug}>
                {p.name || p.slug}
              </Select.Item>
            ))}
          </Select.Content>
        </Select.Root>
        <Select.Root value={status || 'all'} onValueChange={(v) => set('status', v === 'all' ? '' : v)}>
          <Select.Trigger placeholder="Status" />
          <Select.Content>
            <Select.Item value="all">Any status</Select.Item>
            {STATUSES.map((s) => (
              <Select.Item key={s} value={s}>
                {s}
              </Select.Item>
            ))}
          </Select.Content>
        </Select.Root>
        <Select.Root value={builtin ? 'true' : 'false'} onValueChange={(v) => set('builtin', v === 'true' ? 'true' : '')}>
          <Select.Trigger />
          <Select.Content>
            <Select.Item value="false">Work only</Select.Item>
            {/* The brain and the secretary are standing sessions, not work: off by default. */}
            <Select.Item value="true">Include the brain and secretary</Select.Item>
          </Select.Content>
        </Select.Root>
      </Flex>

      <Loaded state={state}>
        {(d) => (
          <>
            <Text size="2" color="gray">
              {d.tasks.length} of {d.total}
            </Text>
            {d.tasks.length === 0 ? (
              <Empty>No task matches.</Empty>
            ) : (
              <Table.Root variant="surface" mt="2">
                <Table.Header>
                  <Table.Row>
                    <Table.ColumnHeaderCell>Task</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Project</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Status</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Agent</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Sessions</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Touched</Table.ColumnHeaderCell>
                  </Table.Row>
                </Table.Header>
                <Table.Body>
                  {d.tasks.map((t) => (
                    <Table.Row key={t.id}>
                      <Table.Cell>
                        <Link className="fleet-row-link" to={`/tasks/${t.id}`}>
                          {t.id}
                        </Link>
                        <Text as="div" size="2" color="gray" className="fleet-wrap">
                          {t.title}
                        </Text>
                      </Table.Cell>
                      <Table.Cell>
                        <Link className="fleet-row-link" to={`/projects/${t.project}`}>
                          <Text size="2">{t.project}</Text>
                        </Link>
                      </Table.Cell>
                      <Table.Cell>
                        <StatusBadge status={t.status} running={t.running} />
                      </Table.Cell>
                      <Table.Cell>
                        <Text size="2">{t.agent ?? '—'}</Text>
                      </Table.Cell>
                      <Table.Cell>
                        {t.sessions ? (
                          <Badge variant="soft">{t.sessions}</Badge>
                        ) : (
                          <Text size="2" color="gray">
                            —
                          </Text>
                        )}
                      </Table.Cell>
                      <Table.Cell>
                        <When at={t.updated_at} />
                      </Table.Cell>
                    </Table.Row>
                  ))}
                </Table.Body>
              </Table.Root>
            )}
          </>
        )}
      </Loaded>
    </>
  )
}
