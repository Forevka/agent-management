import { useState } from 'react'
import {
  Badge, Box, Button, Card, Checkbox, Code, Flex, Grid, Heading, Link as RLink, Table, Text,
  TextArea, TextField,
} from '@radix-ui/themes'
import { Link, useParams } from 'react-router-dom'
import * as api from '../api'
import type { ProjectDetail as Detail } from '../api'
import { ActionButton, Empty, ErrorNote, Loaded, Section, StatusBadge, When, useApi } from '../ui'

export default function ProjectDetail() {
  const { slug = '' } = useParams()
  const path = `/api/projects/${encodeURIComponent(slug)}`
  const state = useApi<Detail>(path)
  const [saving, setSaving] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  const save = async (op: string, body: unknown) => {
    setSaving(op)
    setError(null)
    try {
      await api.act(path, op, body)
      await state.reload()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setSaving(null)
    }
  }

  return (
    <Loaded state={state}>
      {(d) => (
        <>
          <Heading size="6" mt="5" mb="1">
            {d.project.name || d.project.slug}
          </Heading>
          <Text as="p" size="2" color="gray" mb="3">
            <Code variant="ghost">{d.project.slug}</Code>
          </Text>

          {error && <ErrorNote>{error}</ErrorNote>}

          <Grid columns={{ initial: '2', sm: '4' }} gap="3">
            <Card>
              <Text as="div" size="6" weight="bold">
                {d.open_tasks}
              </Text>
              <Text as="div" size="2" color="gray">
                open tasks
              </Text>
            </Card>
            <Card asChild>
              <Link to={`/sessions?project=${d.project.slug}`} style={{ textDecoration: 'none' }}>
                <Text as="div" size="6" weight="bold">
                  {d.sessions}
                </Text>
                <Text as="div" size="2" color="gray">
                  sessions, all time
                </Text>
              </Link>
            </Card>
            <Card>
              <Text as="div" size="6" weight="bold">
                {d.channels.length}
              </Text>
              <Text as="div" size="2" color="gray">
                mapped channels
              </Text>
            </Card>
            <Card>
              <Text as="div" size="6" weight="bold">
                {d.nodes.length}
              </Text>
              <Text as="div" size="2" color="gray">
                nodes with a checkout
              </Text>
            </Card>
          </Grid>

          <Section
            title="Tasks"
            aside={
              <RLink asChild size="2">
                <Link to={`/tasks?project=${d.project.slug}`}>All, with filters →</Link>
              </RLink>
            }
          >
            {d.tasks.length === 0 ? (
              <Empty>No task in this project yet.</Empty>
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

          <Section title="Settings">
            <Card>
              <form
                onSubmit={(e) => {
                  e.preventDefault()
                  const f = new FormData(e.currentTarget)
                  void save('edit', {
                    name: f.get('name'),
                    description: f.get('description'),
                    // Every checkbox is sent, so the list replaces what is bound.
                    folders: d.folders.filter((x) => f.get(`folder-${x.id}`) === 'on').map((x) => x.id),
                  })
                }}
              >
                <Flex direction="column" gap="3">
                  <label>
                    <Text as="div" size="2" mb="1" weight="medium">
                      Name
                    </Text>
                    <TextField.Root name="name" defaultValue={d.project.name} />
                  </label>
                  <label>
                    <Text as="div" size="2" mb="1" weight="medium">
                      Description
                    </Text>
                    <TextArea name="description" defaultValue={d.project.description} />
                  </label>
                  <Box>
                    <Text as="div" size="2" mb="1" weight="medium">
                      Secret folders agents in this project may read
                    </Text>
                    {d.folders.length === 0 ? (
                      <Text size="2" color="gray">
                        No folders yet — make one on the <Link to="/secrets">Secrets</Link> page.
                      </Text>
                    ) : (
                      <Flex direction="column" gap="1">
                        {d.folders.map((f) => (
                          <Text as="label" size="2" key={f.id}>
                            <Flex gap="2" align="center">
                              <Checkbox name={`folder-${f.id}`} defaultChecked={f.bound} />
                              {f.name}
                            </Flex>
                          </Text>
                        ))}
                      </Flex>
                    )}
                  </Box>
                  <Box>
                    <Button type="submit" loading={saving === 'edit'}>
                      Save
                    </Button>
                  </Box>
                </Flex>
              </form>
            </Card>
          </Section>

          <Section title="Jira">
            <Card>
              <Text as="p" size="2" color="gray" mb="3">
                The orchestrator holds the credential and makes the calls; agents never get a token.
                Leave the folder empty to use the shared <Code>jira</Code> folder.
              </Text>
              <form
                onSubmit={(e) => {
                  e.preventDefault()
                  const f = new FormData(e.currentTarget)
                  void save('jira', {
                    key: f.get('key'),
                    board: f.get('board'),
                    folder: f.get('folder'),
                  })
                }}
              >
                <Grid columns={{ initial: '1', sm: '3' }} gap="3">
                  <label>
                    <Text as="div" size="2" mb="1" weight="medium">
                      Project key
                    </Text>
                    <TextField.Root name="key" placeholder="BIOINS" defaultValue={d.project.jira?.key ?? ''} />
                  </label>
                  <label>
                    <Text as="div" size="2" mb="1" weight="medium">
                      Board (optional)
                    </Text>
                    <TextField.Root name="board" placeholder="7" defaultValue={d.project.jira?.board ?? ''} />
                  </label>
                  <label>
                    <Text as="div" size="2" mb="1" weight="medium">
                      Credential folder
                    </Text>
                    <TextField.Root
                      name="folder"
                      placeholder="jira"
                      defaultValue={d.project.jira?.credential_folder ?? ''}
                    />
                  </label>
                </Grid>
                <Box mt="3">
                  <Button type="submit" loading={saving === 'jira'}>
                    Save Jira
                  </Button>
                </Box>
              </form>
            </Card>
          </Section>

          <Section title="Channels">
            <Card>
              {d.channels.length === 0 ? (
                <Empty>No channel maps to this project.</Empty>
              ) : (
                <Flex direction="column" gap="2" mb="3">
                  {d.channels.map((c) => (
                    <Flex key={c.channel_id} gap="2" align="center" wrap="wrap">
                      <Badge variant="soft">{c.source}</Badge>
                      <Text size="2">{c.name}</Text>
                      <Code variant="ghost">{c.channel_id}</Code>
                      {c.triage && (
                        <Badge color="amber" variant="soft">
                          triaged
                        </Badge>
                      )}
                      <Box flexGrow="1" />
                      <ActionButton
                        path={path}
                        op="unchannel"
                        body={{ channel: c.channel_id }}
                        color="red"
                        variant="ghost"
                        confirm={`Stop mapping ${c.name} to this project?`}
                        onDone={() => void state.reload()}
                      >
                        Remove
                      </ActionButton>
                    </Flex>
                  ))}
                </Flex>
              )}
              <form
                onSubmit={(e) => {
                  e.preventDefault()
                  const f = new FormData(e.currentTarget)
                  void save('channel', { channel: f.get('channel'), triage: f.get('triage') === 'on' })
                }}
              >
                <Flex gap="2" align="end" wrap="wrap">
                  <label style={{ flexGrow: 1, minWidth: '14rem' }}>
                    <Text as="div" size="2" mb="1" weight="medium">
                      Add a channel
                    </Text>
                    {/* A name works: the server resolves it against the conversations you are in. */}
                    <TextField.Root name="channel" placeholder="code-reviews, or a channel id" />
                  </label>
                  <Text as="label" size="2">
                    <Flex gap="2" align="center" height="32px">
                      <Checkbox name="triage" />
                      the secretary triages it
                    </Flex>
                  </Text>
                  <Button type="submit" loading={saving === 'channel'}>
                    Add
                  </Button>
                </Flex>
              </form>
            </Card>
          </Section>

          {d.nodes.length > 0 && (
            <Section title="Nodes with a checkout">
              <Flex direction="column" gap="2">
                {d.nodes.map((n) => (
                  <Card key={n.id}>
                    <Flex gap="2" align="center" wrap="wrap">
                      <Badge color={n.online ? 'green' : 'gray'} variant="soft">
                        {n.online ? 'online' : 'offline'}
                      </Badge>
                      <Link className="fleet-row-link" to={`/nodes/${n.id}`}>
                        {n.name}
                      </Link>
                      <Code variant="ghost" className="fleet-wrap">
                        {n.path}
                      </Code>
                    </Flex>
                  </Card>
                ))}
              </Flex>
            </Section>
          )}
        </>
      )}
    </Loaded>
  )
}
