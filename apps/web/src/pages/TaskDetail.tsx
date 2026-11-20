import { useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { SegmentedControl } from '@/components/ui/segmented-control'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Facts, Table } from '@/components/fx/table'
import { ArrowLeftIcon, CheckIcon } from '@/components/fx/icons'
import { AttachForm, AttachmentList } from '@/components/fx/attachments'
import { Note } from '@/components/fx/note'
import { act } from '../api'
import type { Events, Item, TaskDetail as Detail } from '../api'
import { Duration, Loaded, Pre, StatusBadge, When, useApi } from '../ui'

/// Whether an item is still waiting for the owner. An item from an older build of the API may
/// carry no state at all; it is then treated as settled rather than as a false alarm, because
/// the fleet asks again when it still needs something.
function isOpen(i: Item): boolean {
  return i.state === 'open'
}

/// One item of a task, linking to its own page, where the question and the answer are.
function ItemCard({ item, answered }: { item: Item; answered?: boolean }) {
  return (
    <Card as="div" interactive>
      <Link to={`/items/${item.id}`} className="fx-item">
        <Row gap="2">
          <Badge tone={answered ? 'neutral' : item.blocking ? 'danger' : 'warning'} dot>
            {item.kind}
          </Badge>
          <Text size="2" breakAll>
            {item.summary}
          </Text>
          {answered && (
            <Text size="1" tone="muted">
              {item.state}
            </Text>
          )}
        </Row>
      </Link>
    </Card>
  )
}

/// A transcript line. The data is whatever the agent printed — a tool result can carry a file
/// or a Slack message — so it is shown as text and never as markup.
function Event({ kind, at, data }: { kind: string; at: string; data: string }) {
  const [open, setOpen] = useState(false)
  const tone = kind === 'err' ? 'danger' : kind === 'exit' ? 'warning' : 'neutral'
  const oneLine = data.replace(/\s+/g, ' ').slice(0, 200)
  return (
    <div className="fx-event">
      <Row gap="2">
        <Badge tone={tone}>{kind}</Badge>
        <When at={at} />
        <Button size="sm" variant="ghost" onClick={() => setOpen(!open)}>
          {open ? 'less' : 'more'}
        </Button>
      </Row>
      {open ? <Pre>{data}</Pre> : <Text size="1" tone="muted" mono breakAll as="div">{oneLine}</Text>}
    </div>
  )
}

type Tab = 'sessions' | 'checklist' | 'brief' | 'summary' | 'transcript'

export default function TaskDetail() {
  const { id = '' } = useParams()
  const state = useApi<Detail>(`/api/tasks/${encodeURIComponent(id)}`, 10_000)
  const events = useApi<Events>(`/api/tasks/${encodeURIComponent(id)}/events?limit=200`)
  const [tab, setTab] = useState<Tab>('sessions')
  const attachPath = `/api/tasks/${encodeURIComponent(id)}/attachments`

  return (
    <Page>
      <style href="fx-taskdetail" precedence="default">
        {css}
      </style>
      <Loaded state={state}>
        {(d) => (
          <>
            <PageHeader
              title={d.task.id}
              description={d.task.title}
              back={
                <Link to="/tasks" className="fx-link" data-muted="">
                  <ArrowLeftIcon /> All tasks
                </Link>
              }
              meta={<StatusBadge status={d.task.status} running={d.task.running} />}
            />

            <Card>
              <CardContent>
                <Facts
                  items={[
                    [
                      'Project',
                      <Link to={`/projects/${d.task.project}`} className="fx-link">
                        {d.task.project}
                      </Link>,
                    ],
                    ['Agent', d.task.agent ?? '—'],
                    ['Node', d.task.node ?? '—'],
                    [
                      'Runs with',
                      `${d.task.model ?? 'the agent’s model'}${d.task.effort ? ` · ${d.task.effort}` : ''}`,
                    ],
                    ['Created', <When at={d.task.created_at} />],
                  ]}
                />
              </CardContent>
            </Card>

            {d.task.status === 'archived' && (
              <Note tone="info" title="Archived">
                Done, and put away by the nightly archive: hidden from the board, still here. Writing to
                the task brings it back.
              </Note>
            )}

            {/* What the task produced, where it is found again later: artifact and doc links, and
                files the orchestrator keeps (an asset, a migration for devops). */}
            <Section
              title="Attachments"
              description="What this task produced: links and files, kept with it."
            >
              <Card>
                <CardContent>
                  <Stack gap="3">
                    {(d.task.attachments ?? []).length > 0 ? (
                      <AttachmentList
                        items={d.task.attachments ?? []}
                        onRemove={(a) => {
                          if (!window.confirm(`Remove ${a.ref} “${a.title}” from this task?`)) return
                          void act(attachPath, 'remove', { id: a.id })
                            .then(() => void state.reload())
                            .catch((e: unknown) =>
                              window.alert(`Could not remove it: ${e instanceof Error ? e.message : String(e)}`),
                            )
                        }}
                      />
                    ) : (
                      <Text size="1" tone="muted">
                        Nothing yet. Agents attach what they make with <code>fleet agent attach</code>.
                      </Text>
                    )}
                    <AttachForm path={attachPath} onDone={() => void state.reload()} />
                  </Stack>
                </CardContent>
              </Card>
            </Section>

            {/* Open items only: a task keeps its answered questions, and showing those under
                "Waiting on you" told the owner they were holding up work they had already
                answered. The rest are listed below, as a record of what was asked. */}
            {d.items.filter(isOpen).length > 0 && (
              <Section title="Waiting on you">
                <Stack gap="2">
                  {d.items.filter(isOpen).map((i) => (
                    <ItemCard key={i.id} item={i} />
                  ))}
                </Stack>
              </Section>
            )}

            {d.items.filter((i) => !isOpen(i)).length > 0 && (
              <Section title="Answered">
                <Stack gap="2">
                  {d.items
                    .filter((i) => !isOpen(i))
                    .map((i) => (
                      <ItemCard key={i.id} item={i} answered />
                    ))}
                </Stack>
              </Section>
            )}

            <Stack gap="3">
              <SegmentedControl<Tab>
                kind="tabs"
                variant="underline"
                aria-label="What to look at"
                value={tab}
                onChange={setTab}
                options={[
                  { value: 'sessions', label: 'Sessions', count: d.sessions.length },
                  { value: 'checklist', label: 'Checklist', count: d.milestones.length },
                  { value: 'brief', label: 'Brief' },
                  ...(d.task.summary ? [{ value: 'summary' as Tab, label: 'Summary' }] : []),
                  { value: 'transcript', label: 'Transcript' },
                ]}
              />

              {tab === 'sessions' && (
                <Table
                  dense
                  rows={d.sessions}
                  rowKey={(r) => r.run_id}
                  empty="This task has not run yet"
                  columns={[
                    { key: 'started', header: 'Started', cell: (r) => <When at={r.started_at} /> },
                    {
                      key: 'lasted',
                      header: 'Lasted',
                      align: 'end',
                      cell: (r) => <Duration from={r.started_at} to={r.ended_at} />,
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
                    {
                      key: 'session',
                      header: 'Claude session',
                      secondary: true,
                      cell: (r) => (
                        <Text size="1" mono breakAll>
                          {r.session ?? '—'}
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
                          <Text size="1" tone={r.outcome?.startsWith('lost') ? 'warning' : 'muted'}>
                            {r.outcome ?? 'ended'}
                          </Text>
                        ),
                    },
                  ]}
                />
              )}

              {tab === 'checklist' &&
                (d.milestones.length === 0 ? (
                  <EmptyState title="No checklist" text="The agent publishes one after its plan is approved." bare />
                ) : (
                  <Card>
                    <CardContent>
                      <Stack gap="2">
                        {d.milestones.map((m) => (
                          <Row key={m.idx} gap="2" align="start">
                            <span className="fx-check" data-done={m.done || undefined} aria-hidden="true">
                              {m.done ? <CheckIcon /> : null}
                            </span>
                            <Text size="2" tone={m.done ? 'muted' : 'default'} breakAll>
                              {m.title}
                            </Text>
                          </Row>
                        ))}
                      </Stack>
                    </CardContent>
                  </Card>
                ))}

              {tab === 'brief' && <Pre>{d.task.brief ?? ''}</Pre>}

              {tab === 'summary' && <Pre>{d.task.summary ?? ''}</Pre>}

              {tab === 'transcript' && (
                <Loaded state={events}>
                  {(e) =>
                    e.events.length === 0 ? (
                      <EmptyState title="Nothing recorded" bare />
                    ) : (
                      <Card>
                        <CardContent>
                          <Stack gap="0">
                            {e.more && (
                              <Text size="1" tone="muted">
                                The latest {e.events.length} lines.
                              </Text>
                            )}
                            {e.events.map((ev) => (
                              <Event key={ev.id} kind={ev.kind} at={ev.at} data={ev.data} />
                            ))}
                          </Stack>
                        </CardContent>
                      </Card>
                    )
                  }
                </Loaded>
              )}
            </Stack>
          </>
        )}
      </Loaded>
    </Page>
  )
}

const css = /* css */ `
.fx-event {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 8px 0;
  border-bottom: 1px solid var(--aui-border, #e5e5e5);
  min-width: 0;
}
.fx-event:last-child {
  border-bottom: 0;
}

/* A milestone's tick: a circle that fills in, so a finished checklist reads at a glance. */
.fx-check {
  display: grid;
  place-items: center;
  flex: none;
  width: 17px;
  height: 17px;
  margin-top: 2px;
  border: 1.5px solid var(--aui-border-strong, #d4d4d8);
  border-radius: 999px;
  color: transparent;
  font-size: 11px;
  transition:
    background-color 160ms ease,
    border-color 160ms ease,
    color 160ms ease;
}
.fx-check[data-done] {
  background: var(--aui-success, #1e8a4f);
  border-color: var(--aui-success, #1e8a4f);
  color: #fff;
}
`
