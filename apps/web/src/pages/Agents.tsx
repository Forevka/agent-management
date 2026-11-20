import { useState } from 'react'
import { Link } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { Select } from '@/components/ui/select'
import { TextField } from '@/components/ui/text-field'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Table } from '@/components/fx/table'
import * as api from '../api'
import type { Agents as AgentsT } from '../api'
import { ActionButton, ErrorNote, Loaded, useApi } from '../ui'

/// One agent's model and effort. Kept as its own component so each row holds its own draft
/// state and saving one does not disturb the others.
function RunsWith({
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
    <Stack gap="1">
      <Row gap="2" align="end">
        <div style={{ width: '11rem' }}>
          <TextField
            size="sm"
            value={model}
            placeholder={defaultModel}
            list={`models-${agent.id}`}
            aria-label={`Model for ${agent.name}`}
            onChange={(e) => setModel(e.currentTarget.value)}
          />
          <datalist id={`models-${agent.id}`}>
            {suggested.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
        </div>
        <div style={{ width: '9.5rem' }}>
          <Select
            size="sm"
            aria-label={`Effort for ${agent.name}`}
            value={effort || 'default'}
            onChange={(v) => setEffort(v === 'default' ? '' : v)}
            options={[
              { value: 'default', label: 'default effort' },
              ...efforts.map((e) => ({ value: e, label: e })),
            ]}
          />
        </div>
        <Button size="sm" variant="secondary" onClick={() => void save()} loading={busy}>
          Save
        </Button>
      </Row>
      {error && (
        <Text size="1" tone="danger">
          {error}
        </Text>
      )}
    </Stack>
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
    <Page>
      <PageHeader title="Agents" description="Who the fleet can put on a task, and what each one runs with." />
      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) => (
          <>
            <Table
              rows={d.agents}
              rowKey={(a) => a.id}
              loading={state.loading}
              empty="No agent yet"
              columns={[
                {
                  key: 'agent',
                  header: 'Agent',
                  cell: (a) => (
                    <Stack gap="1">
                      <Text weight="medium">{a.name}</Text>
                      <Text size="1" tone="muted" mono>
                        {a.id}
                      </Text>
                    </Stack>
                  ),
                },
                { key: 'role', header: 'Role', cell: (a) => <Badge>{a.role}</Badge> },
                {
                  key: 'project',
                  header: 'Project',
                  secondary: true,
                  cell: (a) =>
                    a.project ? (
                      <Link to={`/projects/${a.project}`} className="fx-link">
                        {a.project}
                      </Link>
                    ) : (
                      <Text size="1" tone="muted">
                        any
                      </Text>
                    ),
                },
                {
                  key: 'runs',
                  header: 'Runs with',
                  width: '28rem',
                  cell: (a) => (
                    <RunsWith
                      agent={a}
                      efforts={d.efforts}
                      defaultModel={d.default_model}
                      suggested={d.suggested_models}
                      onSaved={() => void state.reload()}
                    />
                  ),
                },
              ]}
            />

            <Section
              title="Telegram bots"
              description="The pool of identities agents speak as. A bot’s token is stored encrypted and never shown again."
            >
              <Stack gap="3">
                {d.bots.length === 0 ? (
                  <EmptyState title="No bot in the pool" bare />
                ) : (
                  <Stack gap="2">
                    {d.bots.map((b) => (
                      <Card key={b.id}>
                        <CardContent>
                          <Row gap="2" justify="between">
                            <Row gap="2">
                              <Text weight="medium">@{b.username}</Text>
                              {b.agent ? <Badge tone="info">{b.agent}</Badge> : <Badge>free</Badge>}
                            </Row>
                            <ActionButton
                              path="/api/agents"
                              op="delete"
                              body={{ id: b.id }}
                              variant="ghost"
                              confirm={`Remove @${b.username} from the pool?`}
                              onDone={() => void state.reload()}
                            >
                              Remove
                            </ActionButton>
                          </Row>
                        </CardContent>
                      </Card>
                    ))}
                  </Stack>
                )}
                <Card>
                  <CardHeader title="Add a bot" description="Paste the token BotFather gave you." />
                  <CardContent>
                    <form
                      onSubmit={(e) => {
                        e.preventDefault()
                        void addBot(new FormData(e.currentTarget), e.currentTarget)
                      }}
                    >
                      <Row gap="3" align="end">
                        <div style={{ flexGrow: 1, minWidth: '16rem' }}>
                          <TextField
                            name="token"
                            type="password"
                            autoComplete="off"
                            required
                            size="sm"
                            label="Bot token"
                            hint="Stored encrypted; it is never shown again."
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
            </Section>
          </>
        )}
      </Loaded>
    </Page>
  )
}
