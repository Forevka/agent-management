import { useState } from 'react'
import { Link, Outlet, matchPath, useLocation, useNavigate, useOutletContext, useParams } from 'react-router-dom'
import { Tabs } from '@/components/ui/app-shell'
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
import type { Agents as AgentsT, ProjectDetail as Detail } from '../api'
import { ActionButton, ErrorNote, Loaded, StatusBadge, When, useApi } from '../ui'
import { TaskLink } from '../peek'
import { TaskList } from './Tasks'
import { SessionList } from './Sessions'
import { AgentTable } from './Agents'

/*
 * A project is a directory: `/projects/<slug>` and its sections, each a URL of its own (so the
 * orchestrator can link to exactly one), under one header and one row of tabs. The layout loads the
 * project once and hands it to the section on screen through the outlet's context.
 */

/** The sections, in tab order; the path is what follows `/projects/<slug>`. */
const SECTIONS = [
  ['Overview', ''],
  ['Tasks', 'tasks'],
  ['Sessions', 'sessions'],
  ['Agents', 'agents'],
  ['Settings', 'settings'],
] as const

type Ctx = {
  d: Detail
  path: string
  reload: () => Promise<void>
}

const useProject = () => useOutletContext<Ctx>()

/** How the project stands on a node it is assigned to; the node's page has the details. */
const ASSIGNMENT_LABELS: Record<string, string> = {
  mapped: 'ready',
  cloning: 'cloning',
  failed: 'clone failed',
  waiting: 'clones when a task needs it',
  no_url: 'needs a repository URL',
  node_behind: 'node must update',
}

export function ProjectLayout() {
  const { slug = '' } = useParams()
  const { pathname } = useLocation()
  const navigate = useNavigate()
  const path = `/api/projects/${encodeURIComponent(slug)}`
  // Polled, because the layout stays mounted while the owner moves between its tabs, and the
  // header and the overview's counts should not stay as they were when the page opened.
  const state = useApi<Detail>(path, 30_000)
  const base = `/projects/${encodeURIComponent(slug)}`
  // Matched like the router matches (case aside, decoding included), not cut out of the raw path.
  const section = matchPath({ path: '/projects/:slug/:section' }, pathname)?.params.section?.toLowerCase() ?? ''
  const current = SECTIONS.find(([, p]) => p === section)?.[0] ?? 'Overview'

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
            <Tabs
              items={SECTIONS.map(([label]) => label)}
              value={current}
              onChange={(label) => {
                // The tab already open: nothing to do (and its filters stay).
                if (label === current) return
                const to = SECTIONS.find(([l]) => l === label)?.[1] ?? ''
                navigate(to ? `${base}/${to}` : base)
              }}
            />
            {/* Keyed by the project, so a section's forms are made anew for each one and can never
                hold another project's values. (`useApi` has no data until this project's answer.) */}
            <Outlet key={slug} context={{ d, path, reload: state.reload } satisfies Ctx} />
          </>
        )}
      </Loaded>
    </Page>
  )
}

export function ProjectOverview() {
  const { d } = useProject()
  const base = `/projects/${encodeURIComponent(d.project.slug)}`
  // The server's own list of open work, so it agrees with the count above it; an older
  // orchestrator does not send it, and then the latest tasks are the best there is.
  const open = d.open ?? d.tasks.filter((t) => !['done', 'stopped', 'failed'].includes(t.status))
  return (
    <>
      <Grid min={200} gap="3">
        <Stat label="Open tasks" value={d.open_tasks} to={`${base}/tasks`} />
        <Stat label="Sessions, all time" value={d.sessions} to={`${base}/sessions`} />
        <Stat label="Mapped channels" value={d.channels.length} to={`${base}/settings`} />
        <Stat label="Nodes with a checkout" value={d.nodes.length} />
      </Grid>

      <Section
        title="Open tasks"
        action={
          <Link to={`${base}/tasks`} className="fx-link" data-muted="">
            Every task, with filters →
          </Link>
        }
      >
        <Table
          rows={open}
          rowKey={(t) => t.id}
          empty="Nothing open in this project"
          columns={[
            {
              key: 'task',
              header: 'Task',
              width: '45%',
              cell: (t) => (
                <Stack gap="1">
                  <TaskLink id={t.id}>{t.key && t.key !== t.id ? `${t.key} · ${t.id}` : t.id}</TaskLink>
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

      <Where />
    </>
  )
}

/** Where the project's code is: the nodes it is assigned to, and the ones that have it checked out. */
function Where() {
  const { d } = useProject()
  const assigned = d.assigned ?? []
  if (assigned.length === 0 && d.nodes.length === 0) return null
  return (
    <>
      {assigned.length > 0 && (
        <Section title="Assigned to">
          <Stack gap="2">
            {assigned.map((a) => (
              <Card key={a.node_id}>
                <CardContent>
                  <Row gap="2" justify="between">
                    <Link to={`/nodes/${a.node_id}`} className="fx-link">
                      {a.node}
                    </Link>
                    <Badge
                      tone={
                        a.status === 'mapped'
                          ? 'success'
                          : a.status === 'failed'
                            ? 'danger'
                            : a.status === 'cloning'
                              ? 'info'
                              : 'neutral'
                      }
                    >
                      {ASSIGNMENT_LABELS[a.status] ?? a.status}
                    </Badge>
                  </Row>
                </CardContent>
              </Card>
            ))}
          </Stack>
        </Section>
      )}

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
  )
}

export function ProjectTasks() {
  const { d } = useProject()
  return <TaskList project={d.project.slug} />
}

export function ProjectSessions() {
  const { d } = useProject()
  return <SessionList project={d.project.slug} />
}

export function ProjectAgents() {
  const { d } = useProject()
  const state = useApi<AgentsT>('/api/agents')
  return (
    <Stack gap="3">
      <Text as="p" size="2" tone="muted">
        The agents bound to this project. When one is busy, the fleet gives a task a free sibling of
        the same role, or a new one, up to the limit per project.
      </Text>
      <Loaded state={state}>
        {(a) => (
          <AgentTable
            data={a}
            loading={state.loading}
            project={d.project.slug}
            onSaved={() => void state.reload()}
          />
        )}
      </Loaded>
      <Text size="1" tone="muted">
        Bots and agents of every project are on{' '}
        <Link to="/agents" className="fx-link">
          Agents
        </Link>
        .
      </Text>
    </Stack>
  )
}

export function ProjectSettings() {
  const { d, path, reload } = useProject()
  const [saving, setSaving] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  const save = async (op: string, body: unknown) => {
    setSaving(op)
    setError(null)
    try {
      await api.act(path, op, body)
      await reload()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setSaving(null)
    }
  }

  return (
    <>
      {error && <ErrorNote>{error}</ErrorNote>}

      <Section title="General">
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
                onClick={() => void save('plan_gate', { gate: d.project.plan_gate === 'auto' ? 'owner' : 'auto' })}
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
                        onDone={() => void reload()}
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
                    <TextField name="channel" label="Add a channel" placeholder="code-reviews, or a channel id" size="sm" />
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
    </>
  )
}
