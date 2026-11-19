import { Badge, Box, Card, Flex, Grid, Heading, Link as RLink, Table, Text } from '@radix-ui/themes'
import { Link } from 'react-router-dom'
import { HOLDS_THE_AGENT } from '../api'
import type { Overview } from '../api'
import { Empty, Loaded, Section, StatusBadge, When, useApi } from '../ui'

/// A number with a word under it. The row of these is the whole answer to "how is the fleet".
function Stat({ label, value, to, color }: { label: string; value: number | string; to?: string; color?: 'red' | 'green' }) {
  const inner = (
    <Card>
      <Text as="div" size="6" weight="bold" color={color}>
        {value}
      </Text>
      <Text as="div" size="2" color="gray">
        {label}
      </Text>
    </Card>
  )
  return to ? (
    <Box asChild>
      <Link to={to} style={{ textDecoration: 'none' }}>
        {inner}
      </Link>
    </Box>
  ) : (
    inner
  )
}

export default function Dashboard() {
  // The fleet moves while the page is open, so the dashboard refreshes itself.
  const state = useApi<Overview>('/api/overview', 10_000)
  return (
    <Loaded state={state}>
      {(d) => (
        <>
          <Heading size="6" mt="5" mb="3">
            Dashboard
          </Heading>

          <Grid columns={{ initial: '2', sm: '4' }} gap="3">
            <Stat
              label={d.items.length === 1 ? 'thing waiting on you' : 'things waiting on you'}
              value={d.items.length}
              color={d.items.some((i) => i.blocking) ? 'red' : undefined}
            />
            {/* What is happening this second, not how many tasks are open: a task in progress
                whose agent finished its turn has no session behind it. */}
            <Stat
              label="agents working now"
              value={d.tasks.filter((t) => t.running).length}
              to="/tasks"
              color="green"
            />
            <Stat label="sessions in 24h" value={d.sessions_today} to="/sessions" />
            <Stat label="open todos" value={d.todos_open} to="/inbox" />
          </Grid>

          <Section title="Waiting on you">
            {d.items.length === 0 ? (
              <Empty>Nothing. The fleet is not blocked on you.</Empty>
            ) : (
              <Flex direction="column" gap="2">
                {d.items.map((i) => (
                  <Card key={i.id} asChild>
                    <Link to={`/items/${i.id}`} style={{ textDecoration: 'none' }}>
                      <Flex gap="2" align="center" mb="1" wrap="wrap">
                        <Badge color={i.blocking ? 'red' : 'amber'} variant="soft">
                          {i.kind}
                        </Badge>
                        <Text size="2" color="gray">
                          {i.task ?? i.project ?? ''}
                        </Text>
                        {HOLDS_THE_AGENT.includes(i.kind) && (
                          <Text size="1" color="red">
                            an agent is stopped until you answer
                          </Text>
                        )}
                        <Box flexGrow="1" />
                        <When at={i.created_at} />
                      </Flex>
                      <Text as="div" size="2" className="fleet-wrap">
                        {i.summary}
                      </Text>
                    </Link>
                  </Card>
                ))}
              </Flex>
            )}
          </Section>

          <Section
            title="Open tasks"
            aside={
              <RLink asChild size="2">
                <Link to="/tasks">All tasks, finished included →</Link>
              </RLink>
            }
          >
            {d.tasks.length === 0 ? (
              <Empty>No task is open.</Empty>
            ) : (
              <Table.Root variant="surface">
                <Table.Header>
                  <Table.Row>
                    <Table.ColumnHeaderCell>Task</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Status</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Agent</Table.ColumnHeaderCell>
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
                        <StatusBadge status={t.status} running={t.running} />
                      </Table.Cell>
                      <Table.Cell>
                        <Text size="2">{t.agent ?? '—'}</Text>
                      </Table.Cell>
                      <Table.Cell>
                        <When at={t.updated_at} />
                      </Table.Cell>
                    </Table.Row>
                  ))}
                </Table.Body>
              </Table.Root>
            )}
          </Section>

          <Grid columns={{ initial: '1', sm: '2' }} gap="5">
            <Section title="Projects">
              <Flex direction="column" gap="2">
                {d.projects.map((p) => (
                  <Card key={p.slug} asChild>
                    <Link to={`/projects/${p.slug}`} style={{ textDecoration: 'none' }}>
                      <Flex justify="between" align="center" gap="2">
                        <Box>
                          <Text as="div" weight="medium">
                            {p.name || p.slug}
                          </Text>
                          <Text as="div" size="1" color="gray">
                            {p.slug}
                          </Text>
                        </Box>
                        <Flex gap="2" align="center">
                          {p.running > 0 && (
                            <Badge color="green" variant="soft">
                              {p.running} live
                            </Badge>
                          )}
                          <Badge variant="soft">{p.open} open</Badge>
                          <Text size="1" color="gray">
                            of {p.total}
                          </Text>
                        </Flex>
                      </Flex>
                    </Link>
                  </Card>
                ))}
              </Flex>
            </Section>

            <Section title="Nodes">
              <Flex direction="column" gap="2">
                {d.nodes.map((n) => (
                  <Card key={n.id} asChild>
                    <Link to={`/nodes/${n.id}`} style={{ textDecoration: 'none' }}>
                      <Flex justify="between" align="center" gap="2">
                        <Box>
                          <Text as="div" weight="medium">
                            {n.name}
                          </Text>
                          <Text as="div" size="1" color="gray">
                            {n.os} · {n.version}
                          </Text>
                        </Box>
                        <Flex gap="2" align="center">
                          <Badge color={n.online ? 'green' : 'gray'} variant="soft">
                            {n.online ? 'online' : 'offline'}
                          </Badge>
                          <Text size="2" color="gray">
                            {n.running.length}/{n.max_agents}
                          </Text>
                        </Flex>
                      </Flex>
                    </Link>
                  </Card>
                ))}
              </Flex>
            </Section>
          </Grid>
        </>
      )}
    </Loaded>
  )
}
