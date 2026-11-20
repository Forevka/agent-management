import { useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { Select } from '@/components/ui/select'
import { TextArea } from '@/components/ui/text-area'
import { TextField } from '@/components/ui/text-field'
import { Grid, Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Note } from '@/components/fx/note'
import { Table } from '@/components/fx/table'
import { ArrowLeftIcon } from '@/components/fx/icons'
import * as api from '../api'
import type { Node, NodeDisk } from '../api'
import { DiskBadge } from '../disk'
import { ActionButton, ErrorNote, Loaded, When, useApi } from '../ui'

const AUTH_MODES = ['default', 'oauth_token', 'api_key']

/** Free space on each volume the node's sessions and projects write to, and where it went. */
function DiskSection({ disk }: { disk: NodeDisk | null }) {
  if (!disk) {
    return (
      <EmptyState
        title="Not reported yet"
        text="The node reports its free disk space when it connects and every few minutes."
      />
    )
  }
  return (
    <Stack gap="2">
      {disk.state === 'full' && (
        <Note tone="danger" title="New task runs are held here">
          The disk is almost full. Tasks wait in the queue until space is freed; the fleet&apos;s
          own sessions keep running.
        </Note>
      )}
      {disk.state === 'low' && (
        <Note tone="warning" title="Running low on disk space">
          Below the floor, new task runs on this node will be held.
        </Note>
      )}
      <Table
        rows={disk.volumes}
        rowKey={(v) => v.volume}
        empty="No volume measured"
        columns={[
          {
            key: 'volume',
            header: 'Volume',
            cell: (v) => (
              <Text size="1" mono breakAll>
                {v.volume}
              </Text>
            ),
          },
          {
            key: 'free',
            header: 'Free',
            cell: (v) => (
              <Text size="1">
                {api.formatBytes(v.free)} of {api.formatBytes(v.total)}
              </Text>
            ),
          },
          {
            key: 'what',
            header: 'Holds',
            cell: (v) => (
              <Row gap="1">
                {v.claude_dir && <Badge tone="info">Claude sessions</Badge>}
                {v.projects.map((p) => (
                  <Badge key={p}>{p}</Badge>
                ))}
              </Row>
            ),
          },
        ]}
      />
      {disk.biggest.length > 0 && (
        <Card>
          <CardHeader
            title="Biggest session folders"
            description={`Under ${disk.claude_dir}, one folder per project directory Claude Code has worked in.`}
          />
          <CardContent>
            <Stack gap="1">
              {disk.biggest.map((f) => (
                <Row key={f.name} gap="3" justify="between">
                  <Text size="1" mono breakAll>
                    {f.name}
                  </Text>
                  <Text size="1" tone="muted">
                    {api.formatBytes(f.bytes)}
                  </Text>
                </Row>
              ))}
            </Stack>
          </CardContent>
        </Card>
      )}
      <Row gap="2">
        <DiskBadge disk={disk} />
        <Text size="1" tone="muted">
          reported <When at={disk.at} />
        </Text>
      </Row>
    </Stack>
  )
}

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
    <Page>
      <Loaded state={state}>
        {(n) => (
          <>
            <PageHeader
              title={n.name}
              description={`${n.os} · ${n.version}`}
              back={
                <Link to="/nodes" className="fx-link" data-muted="">
                  <ArrowLeftIcon /> All nodes
                </Link>
              }
              meta={
                <Row gap="2">
                  <Badge tone={n.online ? 'success' : 'neutral'} dot live={n.online || undefined}>
                    {n.online ? 'online' : 'offline'}
                  </Badge>
                  <Text size="1" tone="muted" mono breakAll>
                    {n.id}
                  </Text>
                </Row>
              }
            />

            {error && <ErrorNote>{error}</ErrorNote>}
            {pushError && (
              <Note tone="warning" title="Saved, but the node did not hear it">
                It gets the settings when it next connects.
                <Text as="div" size="1" mono breakAll>
                  {pushError}
                </Text>
              </Note>
            )}

            <Section title="Projects on this machine">
              {n.projects.length === 0 ? (
                <EmptyState
                  title="No project is mapped here"
                  text="Mapping happens on the node itself, in the tray app."
                />
              ) : (
                <Stack gap="2">
                  {n.projects.map((p) => (
                    <Card key={p.project}>
                      <CardContent>
                        <Row gap="3" justify="between">
                          <Link to={`/projects/${p.project}`} className="fx-link">
                            {p.project}
                          </Link>
                          <Text size="1" tone="muted" mono breakAll>
                            {p.path}
                          </Text>
                        </Row>
                      </CardContent>
                    </Card>
                  ))}
                </Stack>
              )}
            </Section>

            <Section title="Disk">
              <DiskSection disk={n.disk} />
            </Section>

            <Section title="Settings">
              <Card>
                <CardHeader
                  title="How this node runs Claude"
                  description="One argument per line. A change is pushed to the node at once when it is connected."
                />
                <CardContent>
                  <form
                    onSubmit={(e) => {
                      e.preventDefault()
                      void save(new FormData(e.currentTarget))
                    }}
                  >
                    <Stack gap="3">
                      <TextArea
                        name="command"
                        label="Command"
                        defaultValue={n.command.join('\n')}
                        minRows={4}
                        maxRows={12}
                      />
                      <Grid min={220} gap="3">
                        <Select
                          label="Authentication"
                          size="sm"
                          value={authMode ?? n.auth_mode}
                          onChange={setAuthMode}
                          options={AUTH_MODES.map((m) => ({ value: m, label: m }))}
                        />
                        <TextField
                          name="max_agents"
                          label="Max agents at once"
                          type="number"
                          min="1"
                          defaultValue={n.max_agents}
                          size="sm"
                        />
                      </Grid>
                      <TextField
                        name="auth_secret"
                        label="Token or key"
                        hint={n.has_auth_secret ? 'One is stored; leave blank to keep it.' : undefined}
                        type="password"
                        autoComplete="off"
                        size="sm"
                        optional
                      />
                      <Row>
                        <Button type="submit" loading={busy} variant="secondary">
                          Save
                        </Button>
                      </Row>
                    </Stack>
                  </form>
                </CardContent>
              </Card>
            </Section>

            <Section
              title="Recent sessions here"
              action={
                <Link to="/sessions" className="fx-link" data-muted="">
                  All sessions →
                </Link>
              }
            >
              <Table
                rows={n.sessions ?? []}
                rowKey={(r) => r.run_id}
                empty="Nothing has run on this node yet"
                columns={[
                  {
                    key: 'task',
                    header: 'Task',
                    cell: (r) => (
                      <Link to={`/tasks/${r.task}`} className="fx-link">
                        {r.task}
                      </Link>
                    ),
                  },
                  { key: 'started', header: 'Started', cell: (r) => <When at={r.started_at} /> },
                  {
                    key: 'status',
                    header: 'Status',
                    align: 'end',
                    cell: (r) => (
                      <Badge tone={r.status === 'running' ? 'success' : 'neutral'} dot live={r.status === 'running' || undefined}>
                        {r.status}
                      </Badge>
                    ),
                  },
                ]}
              />
            </Section>

            <Section title="Danger">
              <Card>
                <CardContent>
                  <Row gap="3" justify="between">
                    <Text size="2" tone="muted">
                      Revoking stops this node's key working and ends anything running on it.
                    </Text>
                    <ActionButton
                      path={path}
                      op="revoke"
                      variant="destructive"
                      confirm={`Revoke ${n.name}? Its key stops working and anything running on it ends.`}
                      onDone={() => navigate('/nodes')}
                    >
                      Revoke this node
                    </ActionButton>
                  </Row>
                </CardContent>
              </Card>
            </Section>
          </>
        )}
      </Loaded>
    </Page>
  )
}
