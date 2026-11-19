import { useState } from 'react'
import {
  Badge, Box, Button, Card, Code, Flex, Grid, Heading, Tabs, Text, TextField,
} from '@radix-ui/themes'
import { Link } from 'react-router-dom'
import * as api from '../api'
import type { Inbox as InboxT } from '../api'
import { ActionButton, Empty, ErrorNote, Loaded, When, useApi } from '../ui'

/// The four todo actions are the same ones the Inbox buttons in Telegram send, by the same code
/// path on the server: done, dropped, mine (make it a task), and open it.
const TODO_ACTIONS: [string, string, string][] = [
  ['d', 'Done', 'gray'],
  ['x', 'Drop', 'red'],
  ['m', 'Make it a task', 'green'],
]

export default function Inbox() {
  const state = useApi<InboxT>('/api/inbox', 20_000)
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
    <>
      <Heading size="6" mt="5" mb="1">
        Inbox
      </Heading>
      <Text as="p" size="2" color="gray" mb="3">
        What the secretary has made of the messages reaching you.
      </Text>
      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) => (
          <>
            <Grid columns={{ initial: '3' }} gap="3" mb="4">
              <Card>
                <Text as="div" size="6" weight="bold">
                  {d.stats.messages_today}
                </Text>
                <Text as="div" size="2" color="gray">
                  messages in 24h
                </Text>
              </Card>
              <Card>
                <Text as="div" size="6" weight="bold">
                  {d.stats.pending}
                </Text>
                <Text as="div" size="2" color="gray">
                  not yet handled
                </Text>
              </Card>
              <Card>
                <Text as="div" size="6" weight="bold">
                  {d.stats.batches_today}
                </Text>
                <Text as="div" size="2" color="gray">
                  secretary runs in 24h
                </Text>
              </Card>
            </Grid>

            <Tabs.Root defaultValue="todos">
              <Tabs.List>
                <Tabs.Trigger value="todos">
                  Todos ({d.todos.filter((t) => t.status === 'open').length})
                </Tabs.Trigger>
                <Tabs.Trigger value="rules">Rules ({d.rules.filter((r) => r.active).length})</Tabs.Trigger>
                <Tabs.Trigger value="triage">Decisions</Tabs.Trigger>
              </Tabs.List>

              <Box pt="3">
                <Tabs.Content value="todos">
                  {d.todos.length === 0 ? (
                    <Empty>Nothing on the list.</Empty>
                  ) : (
                    <Flex direction="column" gap="2">
                      {d.todos.map((t) => (
                        <Card key={t.id} style={{ opacity: t.status === 'open' ? 1 : 0.6 }}>
                          <Flex gap="2" align="center" mb="1" wrap="wrap">
                            {t.status !== 'open' && (
                              <Badge color="gray" variant="soft">
                                {t.status}
                              </Badge>
                            )}
                            <Text weight="medium" className="fleet-wrap">
                              {t.title}
                            </Text>
                            <Box flexGrow="1" />
                            <Text size="1" color="gray">
                              {t.created_by}
                            </Text>
                            <When at={t.created_at} />
                          </Flex>
                          {t.detail && (
                            <Text as="p" size="2" color="gray" className="fleet-wrap" mb="2">
                              {t.detail}
                            </Text>
                          )}
                          {t.workflow && (
                            <Text as="div" size="1" mb="2">
                              Linked to workflow{' '}
                              <Link className="fleet-row-link" to={`/tasks/${t.workflow}`}>
                                {t.workflow}
                              </Link>
                            </Text>
                          )}
                          {t.status === 'open' && (
                            <Flex gap="2" wrap="wrap">
                              {TODO_ACTIONS.map(([action, label, color]) => (
                                <ActionButton
                                  key={action}
                                  path="/api/inbox"
                                  op="todo"
                                  body={{ id: t.id, action }}
                                  color={color as 'red' | 'green' | 'gray'}
                                  variant="soft"
                                  onDone={() => void state.reload()}
                                >
                                  {/* A workflow-linked todo nudges the existing workflow instead
                                      of starting a new task. */}
                                  {action === 'm' && t.workflow ? 'Run the workflow' : label}
                                </ActionButton>
                              ))}
                            </Flex>
                          )}
                        </Card>
                      ))}
                    </Flex>
                  )}
                </Tabs.Content>

                <Tabs.Content value="rules">
                  <Flex direction="column" gap="2" mb="3">
                    {d.rules.length === 0 ? (
                      <Empty>No rule yet.</Empty>
                    ) : (
                      d.rules.map((r) => (
                        <Card key={r.id} style={{ opacity: r.active ? 1 : 0.55 }}>
                          <Flex gap="2" align="center" wrap="wrap">
                            {!r.active && (
                              <Badge color="gray" variant="soft">
                                retired
                              </Badge>
                            )}
                            <Text size="2" className="fleet-wrap">
                              {r.text}
                            </Text>
                            <Box flexGrow="1" />
                            <Text size="1" color="gray">
                              {r.source}
                            </Text>
                            {r.active && (
                              <ActionButton
                                path="/api/inbox"
                                op="rule_remove"
                                body={{ id: r.id }}
                                variant="ghost"
                                color="red"
                                onDone={() => void state.reload()}
                              >
                                Retire
                              </ActionButton>
                            )}
                          </Flex>
                        </Card>
                      ))
                    )}
                  </Flex>
                  <Card>
                    <form
                      onSubmit={(e) => {
                        e.preventDefault()
                        void addRule(new FormData(e.currentTarget), e.currentTarget)
                      }}
                    >
                      <Flex gap="2" align="end" wrap="wrap">
                        <label style={{ flexGrow: 1, minWidth: '18rem' }}>
                          <Text as="div" size="2" mb="1" weight="medium">
                            Tell the secretary a rule
                          </Text>
                          <TextField.Root
                            name="text"
                            placeholder="Escalate anything from the PM about BIOINS straight away"
                            required
                          />
                        </label>
                        <Button type="submit" loading={busy}>
                          Add
                        </Button>
                      </Flex>
                    </form>
                  </Card>
                </Tabs.Content>

                <Tabs.Content value="triage">
                  {d.triage.length === 0 ? (
                    <Empty>No decision recorded.</Empty>
                  ) : (
                    <Flex direction="column" gap="2">
                      {d.triage.map((t, i) => (
                        <Card key={i}>
                          <Flex gap="2" align="center" mb="1" wrap="wrap">
                            <Badge
                              color={t.decision === 'escalate' ? 'amber' : 'gray'}
                              variant="soft"
                            >
                              {t.decision}
                            </Badge>
                            <When at={t.at} />
                            <Box flexGrow="1" />
                            <Text size="1" color="gray" className="fleet-wrap">
                              <Code variant="ghost">{t.refs.join(', ')}</Code>
                            </Text>
                          </Flex>
                          <Text as="div" size="2" className="fleet-wrap">
                            {t.note}
                          </Text>
                        </Card>
                      ))}
                    </Flex>
                  )}
                </Tabs.Content>
              </Box>
            </Tabs.Root>
          </>
        )}
      </Loaded>
    </>
  )
}
