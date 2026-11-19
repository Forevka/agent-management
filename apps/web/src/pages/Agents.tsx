import { useState } from 'react'
import {
  Badge, Box, Button, Card, Code, Flex, Heading, Select, Table, Text, TextField,
} from '@radix-ui/themes'
import { Link } from 'react-router-dom'
import * as api from '../api'
import type { Agents as AgentsT } from '../api'
import { ActionButton, Empty, ErrorNote, Loaded, Section, useApi } from '../ui'

/// One agent's model and effort. Kept as its own component so each row holds its own draft
/// state and saving one does not disturb the others.
function AgentRow({
  agent,
  efforts,
  defaultModel,
  suggested,
  onSaved,
}: {
  agent: AgentsT['agents'][number]
  efforts: string[]
  defaultModel: string
  suggested: string[]
  onSaved: () => void
}) {
  const [model, setModel] = useState(agent.model ?? '')
  const [effort, setEffort] = useState(agent.effort ?? '')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const save = async () => {
    setBusy(true)
    setError(null)
    try {
      await api.act('/api/agents', 'settings', { agent: agent.id, model, effort })
      onSaved()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Table.Row>
      <Table.Cell>
        <Text weight="medium">{agent.name}</Text>
        <Text as="div" size="1" color="gray">
          <Code variant="ghost">{agent.id}</Code>
        </Text>
      </Table.Cell>
      <Table.Cell>
        <Badge variant="soft">{agent.role}</Badge>
      </Table.Cell>
      <Table.Cell>
        {agent.project ? (
          <Link className="fleet-row-link" to={`/projects/${agent.project}`}>
            <Text size="2">{agent.project}</Text>
          </Link>
        ) : (
          <Text size="2" color="gray">
            any
          </Text>
        )}
      </Table.Cell>
      <Table.Cell>
        <Flex gap="2" align="center" wrap="wrap">
          <Box style={{ width: '10rem' }}>
            <TextField.Root
              value={model}
              placeholder={defaultModel}
              list={`models-${agent.id}`}
              onChange={(e) => setModel(e.currentTarget.value)}
            />
            <datalist id={`models-${agent.id}`}>
              {suggested.map((m) => (
                <option key={m} value={m} />
              ))}
            </datalist>
          </Box>
          <Select.Root value={effort || 'default'} onValueChange={(v) => setEffort(v === 'default' ? '' : v)}>
            <Select.Trigger />
            <Select.Content>
              <Select.Item value="default">default effort</Select.Item>
              {efforts.map((e) => (
                <Select.Item key={e} value={e}>
                  {e}
                </Select.Item>
              ))}
            </Select.Content>
          </Select.Root>
          <Button size="2" variant="soft" onClick={() => void save()} loading={busy}>
            Save
          </Button>
        </Flex>
        {error && (
          <Text size="1" color="red">
            {error}
          </Text>
        )}
      </Table.Cell>
    </Table.Row>
  )
}

export default function Agents() {
  const state = useApi<AgentsT>('/api/agents')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const addBot = async (form: FormData, el: HTMLFormElement) => {
    setBusy(true)
    setError(null)
    try {
      await api.act('/api/agents', 'add', { token: form.get('token') })
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
      <Heading size="6" mt="5" mb="3">
        Agents
      </Heading>
      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) => (
          <>
            <Table.Root variant="surface">
              <Table.Header>
                <Table.Row>
                  <Table.ColumnHeaderCell>Agent</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Role</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Project</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Runs with</Table.ColumnHeaderCell>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {d.agents.map((a) => (
                  <AgentRow
                    key={a.id}
                    agent={a}
                    efforts={d.efforts}
                    defaultModel={d.default_model}
                    suggested={d.suggested_models}
                    onSaved={() => void state.reload()}
                  />
                ))}
              </Table.Body>
            </Table.Root>

            <Section title="Telegram bots">
              <Text as="p" size="2" color="gray" mb="2">
                The pool of identities agents speak as. A bot’s token is stored encrypted and never
                shown again.
              </Text>
              {d.bots.length === 0 ? (
                <Empty>No bot in the pool.</Empty>
              ) : (
                <Flex direction="column" gap="2" mb="3">
                  {d.bots.map((b) => (
                    <Card key={b.id}>
                      <Flex gap="2" align="center" wrap="wrap">
                        <Text weight="medium">@{b.username}</Text>
                        {b.agent ? (
                          <Badge variant="soft">{b.agent}</Badge>
                        ) : (
                          <Badge color="gray" variant="soft">
                            free
                          </Badge>
                        )}
                        <Box flexGrow="1" />
                        <ActionButton
                          path="/api/agents"
                          op="delete"
                          body={{ id: b.id }}
                          color="red"
                          variant="ghost"
                          confirm={`Remove @${b.username} from the pool?`}
                          onDone={() => void state.reload()}
                        >
                          Remove
                        </ActionButton>
                      </Flex>
                    </Card>
                  ))}
                </Flex>
              )}
              <Card>
                <form
                  onSubmit={(e) => {
                    e.preventDefault()
                    void addBot(new FormData(e.currentTarget), e.currentTarget)
                  }}
                >
                  <Flex gap="2" align="end" wrap="wrap">
                    <label style={{ flexGrow: 1, minWidth: '16rem' }}>
                      <Text as="div" size="2" mb="1" weight="medium">
                        Add a bot by token
                      </Text>
                      <TextField.Root name="token" type="password" autoComplete="off" required />
                    </label>
                    <Button type="submit" loading={busy}>
                      Add
                    </Button>
                  </Flex>
                </form>
              </Card>
            </Section>
          </>
        )}
      </Loaded>
    </>
  )
}
