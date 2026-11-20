import { useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import { EmptyState } from '@/components/ui/empty-state'
import { TextArea } from '@/components/ui/text-area'
import { TextField } from '@/components/ui/text-field'
import { Grid, Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Stat } from '@/components/fx/stat'
import { Table } from '@/components/fx/table'
import { ArrowLeftIcon } from '@/components/fx/icons'
import * as api from '../api'
import type { ProjectDetail as Detail } from '../api'
import { ActionButton, ErrorNote, Loaded, StatusBadge, When, useApi } from '../ui'

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
    <Page>
      <Loaded state={state}>
        {(d) => (
          <>
            <PageHeader
              title={d.project.name || d.project.slug}
              description={d.project.description || undefined}
              back={
                <Link to="/projects" className="fx-link" data-muted="">
                  <ArrowLeftIcon /> All projects
                </Link>
              }
              meta={
                <Text size="1" tone="muted" mono>
                  {d.project.slug}
                </Text>
              }
            />

            {error && <ErrorNote>{error}</ErrorNote>}

            <Grid min={200} gap="3">
              <Stat label="open tasks" value={d.open_tasks} to={`/tasks?project=${d.project.slug}`} />
              <Stat label="sessions, all time" value={d.sessions} to={`/sessions?project=${d.project.slug}`} />
              <Stat label="mapped channels" value={d.channels.length} />
              <Stat label="nodes with a checkout" value={d.nodes.length} />
            </Grid>

            <Section
              title="Tasks"
              action={
                <Link to={`/tasks?project=${d.project.slug}`} className="fx-link" data-muted="">
                  All, with filters →
                </Link>
              }
            >
              <Table
                rows={d.tasks}
                rowKey={(t) => t.id}
                empty="No task in this project yet"
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
                  { key: 'status', header: 'Status', cell: (t) => <StatusBadge status={t.status} running={t.running} /> },
                  { key: 'agent', header: 'Agent', secondary: true, cell: (t) => <Text size="1">{t.agent ?? '—'}</Text> },
                  { key: 'touched', header: 'Touched', align: 'end', cell: (t) => <When at={t.updated_at} /> },
                ]}
              />
            </Section>

            <Section title="Settings">
              <Card>
                <CardContent>
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
                    <Stack gap="3">
                      <TextField name="name" label="Name" defaultValue={d.project.name} size="sm" />
                      <TextArea name="description" label="Description" defaultValue={d.project.description} minRows={2} />
                      <Stack gap="2">
                        <Text size="2" weight="medium">
                          Secret folders agents in this project may read
                        </Text>
                        {d.folders.length === 0 ? (
                          <Text size="2" tone="muted">
                            No folders yet — make one on the{' '}
                            <Link to="/secrets" className="fx-link">
                              Secrets
                            </Link>{' '}
                            page.
                          </Text>
                        ) : (
                          <Stack gap="1">
                            {d.folders.map((f) => (
                              <Checkbox key={f.id} name={`folder-${f.id}`} defaultChecked={f.bound} label={f.name} />
                            ))}
                          </Stack>
                        )}
                      </Stack>
                      <Row>
                        <Button type="submit" loading={saving === 'edit'} variant="secondary">
                          Save
                        </Button>
                      </Row>
                    </Stack>
                  </form>
                </CardContent>
              </Card>
            </Section>

            <Section title="Repository">
              <Card>
                <CardHeader
                  title="Where this project's code lives"
                  description="Nodes this project is assigned to (on each node's page) clone it from here when a task first needs it, with that machine's own git login. Never put a token in the URL: log in to git on the node once instead."
                />
                <CardContent>
                  <form
                    onSubmit={(e) => {
                      e.preventDefault()
                      const f = new FormData(e.currentTarget)
                      void save('repo_url', { url: f.get('url') })
                    }}
                  >
                    <Row gap="3" align="end">
                      <div style={{ flexGrow: 1, minWidth: '14rem' }}>
                        <TextField
                          name="url"
                          label="Git URL"
                          placeholder="git@github.com:owner/repo.git"
                          defaultValue={d.project.repo_url ?? ''}
                          size="sm"
                          optional
                        />
                      </div>
                      <Button type="submit" loading={saving === 'repo_url'} variant="secondary">
                        Save
                      </Button>
                    </Row>
                  </form>
                </CardContent>
              </Card>
            </Section>

            <Section title="Plan approval">
              <Card>
                <CardHeader
                  title="Who approves this project's plans"
                  description="Every agent plans before it works. With “ask me”, each plan waits for your approval. With “automatic”, the orchestrator approves it and posts it silently in the task’s topic, unless the plan says it needs you. That covers every task here, including the ones the orchestrator creates after reading a ticket or a Slack message. Workflows have their own setting, on their card in Telegram."
                />
                <CardContent>
                  <Row gap="3">
                    <Badge tone={d.project.plan_gate === 'auto' ? 'warning' : 'neutral'}>
                      {d.project.plan_gate === 'auto' ? 'automatic' : 'ask me'}
                    </Badge>
                    <Button
                      variant="secondary"
                      loading={saving === 'plan_gate'}
                      onClick={() =>
                        void save('plan_gate', { gate: d.project.plan_gate === 'auto' ? 'owner' : 'auto' })
                      }
                    >
                      {d.project.plan_gate === 'auto' ? 'Ask me for plans' : 'Approve plans automatically'}
                    </Button>
                  </Row>
                </CardContent>
              </Card>
            </Section>

            <Section title="Jira">
              <Card>
                <CardHeader
                  title="This project's tracker"
                  description="The orchestrator holds the credential and makes the calls; agents get tools, never a token. Leave the folder empty to use the shared jira folder."
                />
                <CardContent>
                  <form
                    onSubmit={(e) => {
                      e.preventDefault()
                      const f = new FormData(e.currentTarget)
                      void save('jira', { key: f.get('key'), board: f.get('board'), folder: f.get('folder') })
                    }}
                  >
                    <Stack gap="3">
                      <Grid min={200} gap="3">
                        <TextField
                          name="key"
                          label="Project key"
                          placeholder="BIOINS"
                          defaultValue={d.project.jira?.key ?? ''}
                          size="sm"
                        />
                        <TextField
                          name="board"
                          label="Board"
                          optional
                          placeholder="7"
                          defaultValue={d.project.jira?.board ?? ''}
                          size="sm"
                        />
                        <TextField
                          name="folder"
                          label="Credential folder"
                          optional
                          placeholder="jira"
                          defaultValue={d.project.jira?.credential_folder ?? ''}
                          size="sm"
                        />
                      </Grid>
                      <Row>
                        <Button type="submit" loading={saving === 'jira'} variant="secondary">
                          Save Jira
                        </Button>
                      </Row>
                    </Stack>
                  </form>
                </CardContent>
              </Card>
            </Section>

            <Section title="Channels">
              <Card>
                <CardContent>
                  <Stack gap="3">
                    {d.channels.length === 0 ? (
                      <EmptyState title="No channel maps to this project" bare />
                    ) : (
                      <Stack gap="2">
                        {d.channels.map((c) => (
                          <Row key={c.channel_id} gap="2" justify="between">
                            <Row gap="2">
                              <Badge>{c.source}</Badge>
                              <Text size="2">{c.name}</Text>
                              <Text size="1" tone="muted" mono>
                                {c.channel_id}
                              </Text>
                              {c.triage && <Badge tone="warning">triaged</Badge>}
                            </Row>
                            <ActionButton
                              path={path}
                              op="unchannel"
                              body={{ channel: c.channel_id }}
                              variant="ghost"
                              confirm={`Stop mapping ${c.name} to this project?`}
                              onDone={() => void state.reload()}
                            >
                              Remove
                            </ActionButton>
                          </Row>
                        ))}
                      </Stack>
                    )}
                    <form
                      onSubmit={(e) => {
                        e.preventDefault()
                        const f = new FormData(e.currentTarget)
                        void save('channel', { channel: f.get('channel'), triage: f.get('triage') === 'on' })
                      }}
                    >
                      <Row gap="3" align="end">
                        <div style={{ flexGrow: 1, minWidth: '14rem' }}>
                          {/* A name works: the server resolves it against the conversations you are in. */}
                          <TextField
                            name="channel"
                            label="Add a channel"
                            placeholder="code-reviews, or a channel id"
                            size="sm"
                          />
                        </div>
                        <Checkbox name="triage" label="the secretary triages it" />
                        <Button type="submit" loading={saving === 'channel'} variant="secondary">
                          Add
                        </Button>
                      </Row>
                    </form>
                  </Stack>
                </CardContent>
              </Card>
            </Section>

            {d.nodes.length > 0 && (
              <Section title="Nodes with a checkout">
                <Stack gap="2">
                  {d.nodes.map((n) => (
                    <Card key={n.id}>
                      <CardContent>
                        <Row gap="2" justify="between">
                          <Row gap="2">
                            <Badge tone={n.online ? 'success' : 'neutral'} dot live={n.online || undefined}>
                              {n.online ? 'online' : 'offline'}
                            </Badge>
                            <Link to={`/nodes/${n.id}`} className="fx-link">
                              {n.name}
                            </Link>
                          </Row>
                          <Text size="1" tone="muted" mono breakAll>
                            {n.path}
                          </Text>
                        </Row>
                      </CardContent>
                    </Card>
                  ))}
                </Stack>
              </Section>
            )}
          </>
        )}
      </Loaded>
    </Page>
  )
}
