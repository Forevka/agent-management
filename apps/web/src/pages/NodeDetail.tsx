import { useState } from 'react'
import {
  Badge, Box, Button, Callout, Card, Code, Flex, Grid, Heading, Select, Table, Text, TextArea,
  TextField,
} from '@radix-ui/themes'
import { Link, useNavigate, useParams } from 'react-router-dom'
import * as api from '../api'
import type { Node } from '../api'
import { ActionButton, Empty, ErrorNote, Loaded, Section, When, useApi } from '../ui'

const AUTH_MODES = ['default', 'oauth_token', 'api_key']

export default function NodeDetail() {
  const { id = '' } = useParams()
  const path = `/api/nodes/${encodeURIComponent(id)}`
  const state = useApi<Node>(path)
  const navigate = useNavigate()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [pushError, setPushError] = useState<string | null>(null)
  const [authMode, setAuthMode] = useState<string | null>(null)

  const save = async (form: FormData) => {
    setBusy(true)
    setError(null)
    setPushError(null)
    try {
      const r = await api.act<{ push_error: string | null }>(path, 'save', {
        command: form.get('command'),
        auth_mode: authMode ?? form.get('auth_mode'),
        max_agents: Number(form.get('max_agents')),
        auth_secret: form.get('auth_secret') || undefined,
      })
      setPushError(r.push_error ?? null)
      await state.reload()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Loaded state={state}>
      {(n) => (
        <>
          <Flex align="center" gap="3" mt="5" mb="1" wrap="wrap">
            <Heading size="6">{n.name}</Heading>
            <Badge color={n.online ? 'green' : 'gray'} variant="soft">
              {n.online ? 'online' : 'offline'}
            </Badge>
          </Flex>
          <Text as="p" size="2" color="gray" mb="3" className="fleet-wrap">
            <Code variant="ghost">{n.id}</Code> · {n.os} · {n.version}
          </Text>

          {error && <ErrorNote>{error}</ErrorNote>}
          {pushError && (
            <Callout.Root color="amber" my="3">
              <Callout.Text>
                Saved, but sending it to the node failed — it gets the settings when it next
                connects.
                <Box mt="1" style={{ fontFamily: 'var(--code-font-family)', fontSize: 'var(--font-size-1)' }}>
                  {pushError}
                </Box>
              </Callout.Text>
            </Callout.Root>
          )}

          <Section title="Projects on this machine">
            {n.projects.length === 0 ? (
              <Empty>
                No project is mapped here. Mapping happens on the node itself, in the tray app.
              </Empty>
            ) : (
              <Flex direction="column" gap="2">
                {n.projects.map((p) => (
                  <Card key={p.project}>
                    <Flex gap="2" align="center" wrap="wrap">
                      <Link className="fleet-row-link" to={`/projects/${p.project}`}>
                        {p.project}
                      </Link>
                      <Code variant="ghost" className="fleet-wrap">
                        {p.path}
                      </Code>
                    </Flex>
                  </Card>
                ))}
              </Flex>
            )}
          </Section>

          <Section title="Settings">
            <Card>
              <form
                onSubmit={(e) => {
                  e.preventDefault()
                  void save(new FormData(e.currentTarget))
                }}
              >
                <Flex direction="column" gap="3">
                  <label>
                    <Text as="div" size="2" mb="1" weight="medium">
                      How Claude is started — one argument per line
                    </Text>
                    <TextArea name="command" defaultValue={n.command.join('\n')} rows={4} />
                  </label>
                  <Grid columns={{ initial: '1', sm: '2' }} gap="3">
                    <Box>
                      <Text as="div" size="2" mb="1" weight="medium">
                        Authentication
                      </Text>
                      <Select.Root
                        value={authMode ?? n.auth_mode}
                        onValueChange={setAuthMode}
                      >
                        <Select.Trigger />
                        <Select.Content>
                          {AUTH_MODES.map((m) => (
                            <Select.Item key={m} value={m}>
                              {m}
                            </Select.Item>
                          ))}
                        </Select.Content>
                      </Select.Root>
                    </Box>
                    <label>
                      <Text as="div" size="2" mb="1" weight="medium">
                        Max agents at once
                      </Text>
                      <TextField.Root name="max_agents" type="number" min="1" defaultValue={n.max_agents} />
                    </label>
                  </Grid>
                  <label>
                    <Text as="div" size="2" mb="1" weight="medium">
                      Token or key {n.has_auth_secret && '— one is stored; leave blank to keep it'}
                    </Text>
                    <TextField.Root name="auth_secret" type="password" autoComplete="off" />
                  </label>
                  <Box>
                    <Button type="submit" loading={busy}>
                      Save
                    </Button>
                  </Box>
                </Flex>
              </form>
            </Card>
          </Section>

          <Section
            title="Recent sessions here"
            aside={
              <Link to={`/sessions`}>
                <Text size="2">All sessions →</Text>
              </Link>
            }
          >
            {!n.sessions || n.sessions.length === 0 ? (
              <Empty>Nothing has run on this node yet.</Empty>
            ) : (
              <Table.Root variant="surface">
                <Table.Header>
                  <Table.Row>
                    <Table.ColumnHeaderCell>Task</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Started</Table.ColumnHeaderCell>
                    <Table.ColumnHeaderCell>Status</Table.ColumnHeaderCell>
                  </Table.Row>
                </Table.Header>
                <Table.Body>
                  {n.sessions.map((r) => (
                    <Table.Row key={r.run_id}>
                      <Table.Cell>
                        <Link className="fleet-row-link" to={`/tasks/${r.task}`}>
                          {r.task}
                        </Link>
                      </Table.Cell>
                      <Table.Cell>
                        <When at={r.started_at} />
                      </Table.Cell>
                      <Table.Cell>
                        <Badge color={r.status === 'running' ? 'green' : 'gray'} variant="soft">
                          {r.status}
                        </Badge>
                      </Table.Cell>
                    </Table.Row>
                  ))}
                </Table.Body>
              </Table.Root>
            )}
          </Section>

          <Section title="Danger">
            <ActionButton
              path={path}
              op="revoke"
              color="red"
              confirm={`Revoke ${n.name}? Its key stops working and anything running on it ends.`}
              onDone={() => navigate('/nodes')}
            >
              Revoke this node
            </ActionButton>
          </Section>
        </>
      )}
    </Loaded>
  )
}
