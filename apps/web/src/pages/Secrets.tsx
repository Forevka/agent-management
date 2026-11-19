import { useState } from 'react'
import {
  Badge, Box, Button, Callout, Card, Code, Flex, Heading, Select, Table, Text, TextField,
} from '@radix-ui/themes'
import { LockClosedIcon } from '@radix-ui/react-icons'
import { Link } from 'react-router-dom'
import * as api from '../api'
import type { Secrets as SecretsT } from '../api'
import { ErrorNote, Loaded, Section, useApi } from '../ui'

export default function Secrets() {
  const state = useApi<SecretsT>('/api/secrets')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [preset, setPreset] = useState('custom')

  const create = async (form: FormData) => {
    setBusy(true)
    setError(null)
    try {
      await api.act('/api/secrets', 'create', { name: form.get('name'), preset })
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
        Secrets
      </Heading>

      <Callout.Root mb="3">
        <Callout.Icon>
          <LockClosedIcon />
        </Callout.Icon>
        <Callout.Text>
          Values are encrypted at rest and never shown again — not here, not to an agent that reads
          them. A folder reaches an agent only as a <Code>.env</Code> file for one run, outside the
          project directory, deleted when the agent exits.
        </Callout.Text>
      </Callout.Root>

      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) => (
          <>
            <Table.Root variant="surface">
              <Table.Header>
                <Table.Row>
                  <Table.ColumnHeaderCell>Folder</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Keys</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Projects</Table.ColumnHeaderCell>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {d.folders.map((f) => (
                  <Table.Row key={f.id}>
                    <Table.Cell>
                      <Link className="fleet-row-link" to={`/secrets/${f.id}`}>
                        {f.name}
                      </Link>
                      {f.preset && f.preset !== 'custom' && (
                        <Text as="div" size="1" color="gray">
                          {f.preset}
                        </Text>
                      )}
                    </Table.Cell>
                    <Table.Cell>
                      <Text size="2">{f.keys}</Text>
                    </Table.Cell>
                    <Table.Cell>
                      <Flex gap="1" wrap="wrap">
                        {f.projects.length === 0 ? (
                          <Text size="2" color="gray">
                            bound to none
                          </Text>
                        ) : (
                          f.projects.map((p) => (
                            <Badge key={p} variant="soft">
                              {p}
                            </Badge>
                          ))
                        )}
                      </Flex>
                    </Table.Cell>
                  </Table.Row>
                ))}
              </Table.Body>
            </Table.Root>

            <Section title="New folder">
              <Card>
                <form
                  onSubmit={(e) => {
                    e.preventDefault()
                    void create(new FormData(e.currentTarget))
                  }}
                >
                  <Flex gap="2" align="end" wrap="wrap">
                    <label style={{ minWidth: '14rem', flexGrow: 1 }}>
                      <Text as="div" size="2" mb="1" weight="medium">
                        Name
                      </Text>
                      <TextField.Root name="name" placeholder="jira" required />
                    </label>
                    <Box>
                      <Text as="div" size="2" mb="1" weight="medium">
                        Preset
                      </Text>
                      {/* A preset only suggests the keys; a folder may hold anything. */}
                      <Select.Root value={preset} onValueChange={setPreset}>
                        <Select.Trigger />
                        <Select.Content>
                          {d.presets.map((p) => (
                            <Select.Item key={p.name} value={p.name}>
                              {p.name}
                              {p.keys.length > 0 ? ` (${p.keys.length} keys)` : ''}
                            </Select.Item>
                          ))}
                        </Select.Content>
                      </Select.Root>
                    </Box>
                    <Button type="submit" loading={busy}>
                      Create
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
