import { Link } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Card } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { Grid, Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Stat } from '@/components/fx/stat'
import { Table } from '@/components/fx/table'
import { HOLDS_THE_AGENT } from '../api'
import type { Overview } from '../api'
import { Loaded, StatusBadge, When, useApi } from '../ui'

export default function Dashboard() {
  // The fleet moves while the page is open, so the dashboard refreshes itself.
  const state = useApi<Overview>('/api/overview', 10_000)
  return (
    <Page>
      <PageHeader title="Dashboard" description="What the fleet is doing, and what it needs from you." />
      <Loaded state={state}>
        {(d) => {
          const blocking = d.items.filter((i) => i.blocking).length
          const live = d.tasks.filter((t) => t.running).length
          return (
            <>
              <Grid min={200} gap="3">
                <Stat
                  label={d.items.length === 1 ? 'thing waiting on you' : 'things waiting on you'}
                  value={d.items.length}
                  hint={blocking > 0 ? `${blocking} holding an agent up` : 'nothing blocking'}
                  tone={blocking > 0 ? 'danger' : 'default'}
                />
                {/* What is happening this second, not how many tasks are open: a task in progress
                    whose agent finished its turn has no session behind it. */}
                <Stat
                  label="agents working now"
                  value={live}
                  hint={live === 0 ? 'every session is resting' : 'live sessions'}
                  tone={live > 0 ? 'success' : 'default'}
                  to="/sessions"
                />
                <Stat label="sessions in 24h" value={d.sessions_today} to="/sessions" />
                <Stat label="open todos" value={d.todos_open} to="/inbox" />
              </Grid>

              <Section title="Waiting on you">
                {d.items.length === 0 ? (
                  <EmptyState title="Nothing is waiting" text="The fleet is not blocked on you." bare />
                ) : (
                  <Stack gap="2">
                    {d.items.map((i) => (
                      <Card key={i.id} as="div" interactive>
                        <Link to={`/items/${i.id}`} className="fx-item">
                          <Row gap="2">
                            <Badge tone={i.blocking ? 'danger' : 'warning'} dot>
                              {i.kind}
                            </Badge>
                            <Text size="1" tone="muted">
                              {i.task ?? i.project ?? ''}
                            </Text>
                            {HOLDS_THE_AGENT.includes(i.kind) && (
                              <Text size="1" tone="danger">
                                an agent is stopped until you answer
                              </Text>
                            )}
                            <span style={{ flexGrow: 1 }} />
                            <When at={i.created_at} />
                          </Row>
                          <Text as="div" size="2" breakAll>
                            {i.summary}
                          </Text>
                        </Link>
                      </Card>
                    ))}
                  </Stack>
                )}
              </Section>

              <Section
                title="Open tasks"
                action={
                  <Link to="/tasks" className="fx-link" data-muted="">
                    All tasks, finished included →
                  </Link>
                }
              >
                <Table
                  rows={d.tasks}
                  rowKey={(t) => t.id}
                  empty="No task is open"
                  columns={[
                    {
                      key: 'task',
                      header: 'Task',
                      width: '45%',
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
                      key: 'status',
                      header: 'Status',
                      cell: (t) => <StatusBadge status={t.status} running={t.running} />,
                    },
                    {
                      key: 'agent',
                      header: 'Agent',
                      secondary: true,
                      cell: (t) => <Text size="1">{t.agent ?? '—'}</Text>,
                    },
                    { key: 'touched', header: 'Touched', align: 'end', cell: (t) => <When at={t.updated_at} /> },
                  ]}
                />
              </Section>

              <Grid min={320} gap="4">
                <Section title="Projects">
                  <Stack gap="2">
                    {d.projects.map((p) => (
                      <Card key={p.slug} as="div" interactive>
                        <Link to={`/projects/${p.slug}`} className="fx-item">
                          <Row justify="between" gap="3">
                            <Stack gap="1">
                              <Text weight="medium">{p.name || p.slug}</Text>
                              <Text size="1" tone="muted" mono>
                                {p.slug}
                              </Text>
                            </Stack>
                            <Row gap="2">
                              {p.running > 0 && (
                                <Badge tone="success" live>
                                  {p.running} live
                                </Badge>
                              )}
                              <Badge>{p.open} open</Badge>
                              <Text size="1" tone="muted">
                                of {p.total}
                              </Text>
                            </Row>
                          </Row>
                        </Link>
                      </Card>
                    ))}
                  </Stack>
                </Section>

                <Section title="Nodes">
                  <Stack gap="2">
                    {d.nodes.map((n) => (
                      <Card key={n.id} as="div" interactive>
                        <Link to={`/nodes/${n.id}`} className="fx-item">
                          <Row justify="between" gap="3">
                            <Stack gap="1">
                              <Text weight="medium">{n.name}</Text>
                              <Text size="1" tone="muted">
                                {n.os} · {n.version}
                              </Text>
                            </Stack>
                            <Row gap="2">
                              <Badge tone={n.online ? 'success' : 'neutral'} dot live={n.online || undefined}>
                                {n.online ? 'online' : 'offline'}
                              </Badge>
                              <Text size="1" tone="muted">
                                {n.running.length}/{n.max_agents}
                              </Text>
                            </Row>
                          </Row>
                        </Link>
                      </Card>
                    ))}
                  </Stack>
                </Section>
              </Grid>
            </>
          )
        }}
      </Loaded>
    </Page>
  )
}
