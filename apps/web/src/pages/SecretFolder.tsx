import { useState } from 'react'
import {
  Badge, Box, Button, Card, Code, Flex, Heading, Table, Text, TextField,
} from '@radix-ui/themes'
import { Link, useNavigate, useParams } from 'react-router-dom'
import * as api from '../api'
import type { SecretFolder as Folder } from '../api'
import { ActionButton, Empty, ErrorNote, Loaded, Section, useApi } from '../ui'

export default function SecretFolder() {
  const { id = '' } = useParams()
  const path = `/api/secrets/${encodeURIComponent(id)}`
  const state = useApi<Folder>(path)
  const navigate = useNavigate()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const set = async (form: FormData, formEl: HTMLFormElement) => {
    setBusy(true)
    setError(null)
    try {
      await api.act(path, 'set', { key: form.get('key'), value: form.get('value') })
      // The value must not stay sitting in the page after it is stored.
      formEl.reset()
      await state.reload()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Loaded state={state}>
      {(d) => {
        const missing = d.expected.filter((k) => !d.keys.some((x) => x.key === k))
        return (
          <>
            <Heading size="6" mt="5" mb="1">
              {d.name}
            </Heading>
            <Flex gap="2" align="center" mb="3" wrap="wrap">
              {d.preset && d.preset !== 'custom' && <Badge variant="soft">{d.preset}</Badge>}
              {d.projects.map((p) => (
                <Link key={p} className="fleet-row-link" to={`/projects/${p}`}>
                  <Badge variant="outline">{p}</Badge>
                </Link>
              ))}
              {d.projects.length === 0 && (
                <Text size="2" color="gray">
                  Bound to no project, so no agent can read it.
                </Text>
              )}
            </Flex>

            {error && <ErrorNote>{error}</ErrorNote>}

            {missing.length > 0 && (
              <Card mb="3">
                <Text size="2">
                  This preset also expects{' '}
                  {missing.map((k, i) => (
                    <span key={k}>
                      {i > 0 && ', '}
                      <Code>{k}</Code>
                    </span>
                  ))}
                  .
                </Text>
              </Card>
            )}

            <Section title="Keys">
              {d.keys.length === 0 ? (
                <Empty>Nothing stored yet.</Empty>
              ) : (
                <Table.Root variant="surface">
                  <Table.Header>
                    <Table.Row>
                      <Table.ColumnHeaderCell>Key</Table.ColumnHeaderCell>
                      <Table.ColumnHeaderCell />
                    </Table.Row>
                  </Table.Header>
                  <Table.Body>
                    {d.keys.map((k) => (
                      <Table.Row key={k.key}>
                        <Table.Cell>
                          <Code>{k.key}</Code>
                        </Table.Cell>
                        <Table.Cell>
                          <ActionButton
                            path={path}
                            op="unset"
                            body={{ key: k.key }}
                            color="red"
                            variant="ghost"
                            confirm={`Remove ${k.key}? Anything using it stops working.`}
                            onDone={() => void state.reload()}
                          >
                            Remove
                          </ActionButton>
                        </Table.Cell>
                      </Table.Row>
                    ))}
                  </Table.Body>
                </Table.Root>
              )}
            </Section>

            <Section title="Set a key">
              <Card>
                <form
                  onSubmit={(e) => {
                    e.preventDefault()
                    void set(new FormData(e.currentTarget), e.currentTarget)
                  }}
                >
                  <Flex gap="2" align="end" wrap="wrap">
                    <label style={{ minWidth: '12rem' }}>
                      <Text as="div" size="2" mb="1" weight="medium">
                        Key
                      </Text>
                      <TextField.Root name="key" placeholder="JIRA_API_TOKEN" required list="expected" />
                      <datalist id="expected">
                        {d.expected.map((k) => (
                          <option key={k} value={k} />
                        ))}
                      </datalist>
                    </label>
                    <label style={{ minWidth: '16rem', flexGrow: 1 }}>
                      <Text as="div" size="2" mb="1" weight="medium">
                        Value
                      </Text>
                      {/* A password field, and never read back: setting it again is the only way
                          to change it. */}
                      <TextField.Root name="value" type="password" autoComplete="off" required />
                    </label>
                    <Button type="submit" loading={busy}>
                      Store
                    </Button>
                  </Flex>
                </form>
              </Card>
            </Section>

            <Section title="Danger">
              <ActionButton
                path={path}
                op="delete"
                color="red"
                confirm={`Delete the folder ${d.name} and everything in it?`}
                onDone={() => navigate('/secrets')}
              >
                Delete this folder
              </ActionButton>
            </Section>

            <Box mt="4">
              <Link to="/secrets">← All folders</Link>
            </Box>
          </>
        )
      }}
    </Loaded>
  )
}
