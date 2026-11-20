import { useState } from 'react'
import {
  Badge, Box, Button, Card, Code, Flex, Heading, Table, Tabs, Text,
} from '@radix-ui/themes'
import { CheckIcon } from '@radix-ui/react-icons'
import { Link, useParams } from 'react-router-dom'
import type { Events, Item, TaskDetail as Detail } from '../api'
import { Duration, Empty, Loaded, Pre, Section, StatusBadge, When, useApi } from '../ui'

/// Whether an item is still waiting for the owner. An item from an older build of the API may
/// carry no state at all; it is then treated as settled rather than as a false alarm, because
/// the fleet asks again when it still needs something.
function isOpen(i: Item): boolean {
  return i.state === 'open'
}

/// One item of a task, linking to its own page, where the question and the answer are.
function ItemCard({ item, answered }: { item: Item; answered?: boolean }) {
  return (
    <Card asChild>
      <Link to={`/items/${item.id}`} style={{ textDecoration: 'none' }}>
        <Flex gap="2" align="center" wrap="wrap">
          <Badge color={answered ? 'gray' : item.blocking ? 'red' : 'amber'} variant="soft">
            {item.kind}
          </Badge>
          <Text size="2" className="fleet-wrap">
            {item.summary}
          </Text>
          {answered ? (
            <Text size="1" color="gray">
              {item.state}
            </Text>
          ) : null}
        </Flex>
      </Link>
    </Card>
  )
}

/// A transcript line. The data is whatever the agent printed — a tool result can carry a file
/// or a Slack message — so it is shown as text and never as markup.
function Event({ kind, at, data }: { kind: string; at: string; data: string }) {
  const [open, setOpen] = useState(false)
  const color = kind === 'err' ? 'red' : kind === 'exit' ? 'amber' : 'gray'
  const oneLine = data.replace(/\s+/g, ' ').slice(0, 200)
  return (
    <Box py="1" style={{ borderBottom: '1px solid var(--gray-a3)' }}>
      <Flex gap="2" align="center" wrap="wrap">
        <Badge color={color} variant="soft">
          {kind}
        </Badge>
        <When at={at} />
        <Button size="1" variant="ghost" onClick={() => setOpen(!open)}>
          {open ? 'less' : 'more'}
        </Button>
      </Flex>
      {open ? (
        <Box mt="1">
          <Pre>{data}</Pre>
        </Box>
      ) : (
        <Text as="div" size="1" color="gray" className="fleet-wrap" style={{ fontFamily: 'var(--code-font-family)' }}>
          {oneLine}
        </Text>
      )}
    </Box>
  )
}

export default function TaskDetail() {
  const { id = '' } = useParams()
  const state = useApi<Detail>(`/api/tasks/${encodeURIComponent(id)}`, 10_000)
  const events = useApi<Events>(`/api/tasks/${encodeURIComponent(id)}/events?limit=200`)

  return (
    <Loaded state={state}>
      {(d) => (
        <>
          <Flex align="center" gap="3" mt="5" mb="1" wrap="wrap">
            <Heading size="6">{d.task.id}</Heading>
            <StatusBadge status={d.task.status} running={d.task.running} />
          </Flex>
          <Text as="p" size="3" mb="3" className="fleet-wrap">
            {d.task.title}
          </Text>

          <Card>
            <Flex gap="5" wrap="wrap">
              <Box>
                <Text as="div" size="1" color="gray">
                  Project
                </Text>
                <Link className="fleet-row-link" to={`/projects/${d.task.project}`}>
                  {d.task.project}
                </Link>
              </Box>
              <Box>
                <Text as="div" size="1" color="gray">
                  Agent
                </Text>
                <Text size="2">{d.task.agent ?? '—'}</Text>
              </Box>
              <Box>
                <Text as="div" size="1" color="gray">
                  Node
                </Text>
                <Text size="2" className="fleet-wrap">
                  {d.task.node ?? '—'}
                </Text>
              </Box>
              <Box>
                <Text as="div" size="1" color="gray">
                  Runs with
                </Text>
                <Text size="2">
                  {d.task.model ?? 'the agent’s model'}
                  {d.task.effort ? ` · ${d.task.effort}` : ''}
                </Text>
              </Box>
              <Box>
                <Text as="div" size="1" color="gray">
                  Created
                </Text>
                <When at={d.task.created_at} />
              </Box>
            </Flex>
          </Card>

          {/* Open items only: a task keeps its answered questions, and showing those under
              "Waiting on you" told the owner they were holding up work they had already
              answered. The rest are listed below, as a record of what was asked. */}
          {d.items.filter(isOpen).length > 0 && (
            <Section title="Waiting on you">
              <Flex direction="column" gap="2">
                {d.items.filter(isOpen).map((i) => (
                  <ItemCard key={i.id} item={i} />
                ))}
              </Flex>
            </Section>
          )}

          {d.items.filter((i) => !isOpen(i)).length > 0 && (
            <Section title="Answered">
              <Flex direction="column" gap="2">
                {d.items
                  .filter((i) => !isOpen(i))
                  .map((i) => (
                    <ItemCard key={i.id} item={i} answered />
                  ))}
              </Flex>
            </Section>
          )}

          <Tabs.Root defaultValue="sessions" mt="5">
            <Tabs.List>
              <Tabs.Trigger value="sessions">Sessions ({d.sessions.length})</Tabs.Trigger>
              <Tabs.Trigger value="checklist">Checklist ({d.milestones.length})</Tabs.Trigger>
              <Tabs.Trigger value="brief">Brief</Tabs.Trigger>
              <Tabs.Trigger value="transcript">Transcript</Tabs.Trigger>
            </Tabs.List>

            <Box pt="3">
              <Tabs.Content value="sessions">
                {d.sessions.length === 0 ? (
                  <Empty>This task has not run yet.</Empty>
                ) : (
                  <Table.Root variant="surface">
                    <Table.Header>
                      <Table.Row>
                        <Table.ColumnHeaderCell>Started</Table.ColumnHeaderCell>
                        <Table.ColumnHeaderCell>Lasted</Table.ColumnHeaderCell>
                        <Table.ColumnHeaderCell>Node</Table.ColumnHeaderCell>
                        <Table.ColumnHeaderCell>Claude session</Table.ColumnHeaderCell>
                        <Table.ColumnHeaderCell>Ended</Table.ColumnHeaderCell>
                      </Table.Row>
                    </Table.Header>
                    <Table.Body>
                      {d.sessions.map((r) => (
                        <Table.Row key={r.run_id}>
                          <Table.Cell>
                            <When at={r.started_at} />
                          </Table.Cell>
                          <Table.Cell>
                            <Duration from={r.started_at} to={r.ended_at} />
                          </Table.Cell>
                          <Table.Cell>
                            <Text size="2" className="fleet-wrap">
                              {r.node ?? '—'}
                            </Text>
                          </Table.Cell>
                          <Table.Cell>
                            <Text size="1" className="fleet-wrap">
                              <Code variant="ghost">{r.session ?? '—'}</Code>
                            </Text>
                          </Table.Cell>
                          <Table.Cell>
                            {r.status === 'running' ? (
                              <Badge color="green" variant="solid">
                                live
                              </Badge>
                            ) : (
                              <Text size="2" color={r.outcome?.startsWith('lost') ? 'amber' : 'gray'}>
                                {r.outcome ?? 'ended'}
                              </Text>
                            )}
                          </Table.Cell>
                        </Table.Row>
                      ))}
                    </Table.Body>
                  </Table.Root>
                )}
              </Tabs.Content>

              <Tabs.Content value="checklist">
                {d.milestones.length === 0 ? (
                  <Empty>No checklist.</Empty>
                ) : (
                  <Flex direction="column" gap="1">
                    {d.milestones.map((m) => (
                      <Flex key={m.idx} gap="2" align="center">
                        <Text color={m.done ? 'green' : 'gray'}>{m.done ? <CheckIcon /> : '○'}</Text>
                        <Text size="2" color={m.done ? 'gray' : undefined} className="fleet-wrap">
                          {m.title}
                        </Text>
                      </Flex>
                    ))}
                  </Flex>
                )}
              </Tabs.Content>

              <Tabs.Content value="brief">
                <Pre>{d.task.brief ?? ''}</Pre>
              </Tabs.Content>

              <Tabs.Content value="transcript">
                <Loaded state={events}>
                  {(e) =>
                    e.events.length === 0 ? (
                      <Empty>Nothing recorded.</Empty>
                    ) : (
                      <Box>
                        {e.more && (
                          <Text size="1" color="gray">
                            The latest {e.events.length} lines.
                          </Text>
                        )}
                        {e.events.map((ev) => (
                          <Event key={ev.id} kind={ev.kind} at={ev.at} data={ev.data} />
                        ))}
                      </Box>
                    )
                  }
                </Loaded>
              </Tabs.Content>
            </Box>
          </Tabs.Root>
        </>
      )}
    </Loaded>
  )
}
