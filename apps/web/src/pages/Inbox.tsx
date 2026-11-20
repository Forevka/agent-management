import { useState } from 'react'
import { Link } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardFooter, CardHeader } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { SegmentedControl } from '@/components/ui/segmented-control'
import { TextField } from '@/components/ui/text-field'
import { Grid, Page, PageHeader, Row, Stack, Text } from '@/components/fx/layout'
import { Stat } from '@/components/fx/stat'
import * as api from '../api'
import type { Inbox as InboxT } from '../api'
import { ActionButton, ErrorNote, Loaded, When, useApi } from '../ui'

/// The three todo actions are the same ones the Inbox buttons in Telegram send, by the same code
/// path on the server: done, dropped, and mine (make it a task).
const TODO_ACTIONS: [string, string, 'ghost' | 'secondary' | 'primary' | 'destructive'][] = [
  ['d', 'Done', 'secondary'],
  ['x', 'Drop', 'destructive'],
  ['m', 'Make it a task', 'primary'],
]

type Tab = 'todos' | 'rules' | 'triage'

export default function Inbox() {
  const state = useApi<InboxT>('/api/inbox', 20_000)
  const [tab, setTab] = useState<Tab>('todos')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const addRule = async (form: FormData, el: HTMLFormElement) => {
    setBusy(true)
    setError(null)
    try {
      await api.act('/api/inbox', 'rule_add', { text: form.get('text') })
      el.reset()
      await state.reload()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Page>
      <PageHeader title="Inbox" description="What the secretary has made of the messages reaching you." />
      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) => (
          <>
            <Grid min={200} gap="3">
              <Stat label="messages in 24h" value={d.stats.messages_today} />
              <Stat
                label="not yet handled"
                value={d.stats.pending}
                tone={d.stats.pending > 0 ? 'warning' : 'default'}
              />
              <Stat label="secretary runs in 24h" value={d.stats.batches_today} />
            </Grid>

            <Stack gap="3">
              <SegmentedControl<Tab>
                kind="tabs"
                variant="underline"
                aria-label="Inbox sections"
                value={tab}
                onChange={setTab}
                options={[
                  { value: 'todos', label: 'Todos', count: d.todos.filter((t) => t.status === 'open').length },
                  { value: 'rules', label: 'Rules', count: d.rules.filter((r) => r.active).length },
                  { value: 'triage', label: 'Decisions' },
                ]}
              />

              {tab === 'todos' &&
                (d.todos.length === 0 ? (
                  <EmptyState title="Nothing on the list" />
                ) : (
                  <Stack gap="2">
                    {d.todos.map((t) => (
                      <Card key={t.id} style={{ opacity: t.status === 'open' ? 1 : 0.62 }}>
                        <CardHeader
                          title={t.title}
                          description={t.detail || undefined}
                          action={
                            <Row gap="2" justify="end">
                              {t.status !== 'open' && <Badge>{t.status}</Badge>}
                              <Text size="1" tone="muted">
                                {t.created_by}
                              </Text>
                              <When at={t.created_at} />
                            </Row>
                          }
                        />
                        {t.workflow && (
                          <CardContent>
                            <Text size="1">
                              Linked to workflow{' '}
                              <Link to={`/tasks/${t.workflow}`} className="fx-link">
                                {t.workflow}
                              </Link>
                            </Text>
                          </CardContent>
                        )}
                        {t.status === 'open' && (
                          <CardFooter>
                            <Row gap="2">
                              {TODO_ACTIONS.map(([action, label, variant]) => (
                                <ActionButton
                                  key={action}
                                  path="/api/inbox"
                                  op="todo"
                                  body={{ id: t.id, action }}
                                  variant={variant}
                                  onDone={() => void state.reload()}
                                >
                                  {/* A workflow-linked todo nudges the existing workflow instead
                                      of starting a new task. */}
                                  {action === 'm' && t.workflow ? 'Run the workflow' : label}
                                </ActionButton>
                              ))}
                            </Row>
                          </CardFooter>
                        )}
                      </Card>
                    ))}
                  </Stack>
                ))}

              {tab === 'rules' && (
                <Stack gap="3">
                  {d.rules.length === 0 ? (
                    <EmptyState title="No rule yet" bare />
                  ) : (
                    <Stack gap="2">
                      {d.rules.map((r) => (
                        <Card key={r.id} style={{ opacity: r.active ? 1 : 0.55 }}>
                          <CardContent>
                            <Row gap="2" justify="between">
                              <Row gap="2">
                                {!r.active && <Badge>retired</Badge>}
                                <Text size="2" breakAll>
                                  {r.text}
                                </Text>
                              </Row>
                              <Row gap="2">
                                <Text size="1" tone="muted">
                                  {r.source}
                                </Text>
                                {r.active && (
                                  <ActionButton
                                    path="/api/inbox"
                                    op="rule_remove"
                                    body={{ id: r.id }}
                                    variant="ghost"
                                    onDone={() => void state.reload()}
                                  >
                                    Retire
                                  </ActionButton>
                                )}
                              </Row>
                            </Row>
                          </CardContent>
                        </Card>
                      ))}
                    </Stack>
                  )}
                  <Card>
                    <CardHeader
                      title="Tell the secretary a rule"
                      description="It applies to everything it triages from now on."
                    />
                    <CardContent>
                      <form
                        onSubmit={(e) => {
                          e.preventDefault()
                          void addRule(new FormData(e.currentTarget), e.currentTarget)
                        }}
                      >
                        <Row gap="3" align="end">
                          <div style={{ flexGrow: 1, minWidth: '18rem' }}>
                            <TextField
                              name="text"
                              size="sm"
                              placeholder="Escalate anything from the PM about BIOINS straight away"
                              required
                              aria-label="New rule"
                            />
                          </div>
                          <Button type="submit" loading={busy} variant="secondary">
                            Add
                          </Button>
                        </Row>
                      </form>
                    </CardContent>
                  </Card>
                </Stack>
              )}

              {tab === 'triage' &&
                (d.triage.length === 0 ? (
                  <EmptyState title="No decision recorded" bare />
                ) : (
                  <Stack gap="2">
                    {d.triage.map((t, i) => (
                      <Card key={i}>
                        <CardContent>
                          <Stack gap="2">
                            <Row gap="2" justify="between">
                              <Row gap="2">
                                <Badge tone={t.decision === 'escalate' ? 'warning' : 'neutral'} dot>
                                  {t.decision}
                                </Badge>
                                <When at={t.at} />
                              </Row>
                              <Text size="1" tone="muted" mono breakAll>
                                {t.refs.join(', ')}
                              </Text>
                            </Row>
                            <Text size="2" breakAll>
                              {t.note}
                            </Text>
                          </Stack>
                        </CardContent>
                      </Card>
                    ))}
                  </Stack>
                ))}
            </Stack>
          </>
        )}
      </Loaded>
    </Page>
  )
}
