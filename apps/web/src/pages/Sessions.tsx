import { useMemo } from 'react'
import { Badge, Box, Code, Flex, Heading, Select, Table, Text } from '@radix-ui/themes'
import { Link, useSearchParams } from 'react-router-dom'
import type { Overview, Session } from '../api'
import { Duration, Empty, Loaded, When, useApi } from '../ui'

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
    <>
      <Heading size="6" mt="5" mb="1">
        Sessions
      </Heading>
      <Text as="p" size="2" color="gray" mb="3">
        One row per agent process the fleet has started.
      </Text>

      <Flex gap="2" wrap="wrap" mb="3">
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
      </Flex>

      <Loaded state={state}>
        {(d) => (
          <>
            <Text size="2" color="gray">
              {d.runs.length} of {d.total}
            </Text>
            {d.runs.length === 0 ? (
              <Empty>No session yet.</Empty>
            ) : (
              <Table.Root variant="surface" mt="2">
                <Table.Header>
                  <Table.Row>
                    <Table.ColumnHeaderCell>Task</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Agent</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Node</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Started</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Lasted</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Ended</Table.ColumnHeaderCell>
                  </Table.Row>
                </Table.Header>
                <Table.Body>
                  {d.runs.map((r) => (
                    <Table.Row key={r.run_id}>
                      <Table.Cell>
                        <Link className="fleet-row-link" to={`/tasks/${r.task}`}>
                          {r.task}
                        </Link>
                        <Text as="div" size="2" color="gray" className="fleet-wrap">
                          {r.title ?? ''}
                        </Text>
                      </Table.Cell>
                      <Table.Cell>
                        <Text size="2">{r.agent ?? '—'}</Text>
                        <Text as="div" size="1" color="gray">
                          {r.project}
                        </Text>
                      </Table.Cell>
                      <Table.Cell>
                        <Text size="2" className="fleet-wrap">
                          {r.node ?? '—'}
                        </Text>
                      </Table.Cell>
                      <Table.Cell>
                        <When at={r.started_at} />
                      </Table.Cell>
                      <Table.Cell>
                        <Duration from={r.started_at} to={r.ended_at} />
                      </Table.Cell>
                      <Table.Cell>
                        {r.status === 'running' ? (
                          <Badge color="green" variant="solid">
                            live
                          </Badge>
                        ) : (
                          <Box>
                            {/* Either an exit code or why it was lost; both are worth seeing. */}
                            <Text size="2" color={r.outcome?.startsWith('lost') ? 'amber' : 'gray'}>
                              {r.outcome ?? 'ended'}
                            </Text>
                            {r.session && (
                              <Text as="div" size="1" color="gray" className="fleet-wrap" title="Claude's session id">
                                <Code variant="ghost">{r.session.slice(0, 8)}</Code>
                              </Text>
                            )}
                          </Box>
                        )}
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
