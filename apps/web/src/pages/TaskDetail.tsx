import { useRef, useState } from 'react'
import { Link, useNavigate, useParams, useSearchParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { SegmentedControl } from '@/components/ui/segmented-control'
import { Select } from '@/components/ui/select'
import { TextArea } from '@/components/ui/text-area'
import { TextField } from '@/components/ui/text-field'
import { Grid, Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Facts, Table } from '@/components/fx/table'
import { ArrowLeftIcon, CheckIcon, ExternalIcon } from '@/components/fx/icons'
import { AttachForm, AttachmentList } from '@/components/fx/attachments'
import { Note } from '@/components/fx/note'
import * as api from '../api'
import type { Agents, Events, Item, Task, TaskDetail as Detail, TaskLink } from '../api'
import { ActionButton, Duration, ErrorNote, Loaded, Pre, StatusBadge, When, useApi } from '../ui'

/// The fleet's own standing sessions. They are not work the owner edits, stops or writes to from
/// here (the server refuses it too); they have their own resets in Telegram.
const BUILTIN = ['brain', 'secretary', 'retro']

/// Over, as far as the fleet is concerned: a message resumes it, an edit does not.
const FINISHED = ['done', 'stopped', 'failed']

const message = (e: unknown) => (e instanceof Error ? e.message : String(e))

/// A link's URL, only when it is one a browser should open. The server only ever stores http(s)
/// URLs, but a link is text someone else wrote, so the page checks again rather than trust it.
const safeUrl = (url: string | null) => (url && /^https?:\/\//i.test(url) ? url : null)

const LINK_KINDS: Record<string, string> = { jira: 'ticket', mr: 'merge request', pr: 'pull request', url: 'page' }

/// The owner's edit of a task. The brief only reaches a session that has not started yet, so a
/// finished task whose agent already read it cannot take a new one: a message is the way to
/// resume it, and the form says so instead of offering a field that would be refused.
function EditTask({
  task,
  onSaved,
  onCancel,
}: {
  task: Task
  onSaved: (done: string, warning: boolean) => void
  onCancel: () => void
}) {
  // The same lists the agents page offers: any effort level, and the model as free text with the
  // fleet's suggestions (the owner is not held to the brain's allow-list).
  const agents = useApi<Agents>('/api/agents')
  const [effort, setEffort] = useState(task.effort ?? '')
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // Decided once, when the form opens: the page polls, and a task finishing while the owner
  // writes must not take the field (and their draft) away. If it did finish, the server refuses
  // the brief and the form keeps what they wrote.
  const [briefLocked] = useState(() => FINISHED.includes(task.status) && task.session !== null)

  return (
    <Card>
      <CardHeader
        title="Edit task"
        description="The model and effort apply from its next run. A new brief is sent to its agent if its session has already started."
      />
      <CardContent>
        <form
          onSubmit={(e) => {
            e.preventDefault()
            const f = new FormData(e.currentTarget)
            setSaving(true)
            setError(null)
            api
              .act<{ done: string; brief_error: string | null }>(`/api/tasks/${encodeURIComponent(task.id)}`, 'edit', {
                title: f.get('title'),
                ...(briefLocked ? {} : { brief: f.get('brief') }),
                model: f.get('model'),
                effort,
              })
              .then((r) => onSaved(r.done, r.brief_error != null))
              .catch((err) => setError(message(err)))
              .finally(() => setSaving(false))
          }}
        >
          <Stack gap="3">
            <TextField name="title" label="Title" defaultValue={task.title} size="sm" required />
            <TextArea
              name="brief"
              label="Brief"
              defaultValue={task.brief ?? ''}
              minRows={4}
              maxRows={16}
              disabled={briefLocked}
              hint={
                briefLocked
                  ? `This task is ${task.status} and its agent has read the brief; send it a message instead, which resumes it.`
                  : undefined
              }
            />
            <Grid min={200} gap="3">
              <div>
                <TextField
                  name="model"
                  label="Model"
                  size="sm"
                  defaultValue={task.model ?? ''}
                  placeholder="the agent’s model"
                  list="task-models"
                  hint="Empty: the agent’s own."
                />
                <datalist id="task-models">
                  {(agents.data?.suggested_models ?? []).map((m) => (
                    <option key={m} value={m} />
                  ))}
                </datalist>
              </div>
              <Select
                label="Effort"
                size="sm"
                value={effort || 'default'}
                onChange={(v) => setEffort(v === 'default' ? '' : v)}
                options={[
                  { value: 'default', label: 'the agent’s effort' },
                  // The task's current value stays selectable while the list loads.
                  ...Array.from(new Set([...(agents.data?.efforts ?? []), ...(task.effort ? [task.effort] : [])])).map(
                    (e) => ({ value: e, label: e }),
                  ),
                ]}
              />
            </Grid>
            {error && <ErrorNote>{error}</ErrorNote>}
            <Row gap="2">
              <Button type="submit" loading={saving}>
                Save
              </Button>
              <Button type="button" variant="ghost" onClick={onCancel}>
                Cancel
              </Button>
            </Row>
          </Stack>
        </form>
      </CardContent>
    </Card>
  )
}

/// Writing to the task's agent, as a message in its Telegram topic does (it is shown there too).
function MessageBox({ task, onSent }: { task: Task; onSent: () => void }) {
  const [text, setText] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [done, setDone] = useState<string | null>(null)
  // Ctrl+Enter and the button both send, and a state update lands only on the next render: the
  // ref is what stops a second press from sending the message twice.
  const inFlight = useRef(false)
  const send = () => {
    if (!text.trim() || busy || inFlight.current) return
    inFlight.current = true
    setBusy(true)
    setError(null)
    setDone(null)
    api
      .act<{ done: string }>(`/api/tasks/${encodeURIComponent(task.id)}`, 'send', { text })
      .then((r) => {
        setText('')
        setDone(r.done)
        onSent()
      })
      .catch((err) => setError(message(err)))
      .finally(() => {
        inFlight.current = false
        setBusy(false)
      })
  }
  return (
    <Stack gap="2">
      <TextArea
        aria-label="Message to the agent"
        placeholder={FINISHED.includes(task.status) ? 'A follow-up resumes the task…' : 'Write to the agent…'}
        value={text}
        minRows={2}
        maxRows={10}
        onChange={(e) => setText(e.currentTarget.value)}
        onSubmitShortcut={() => send()}
      />
      <Row gap="2">
        <Button size="sm" variant="secondary" onClick={send} loading={busy} disabled={!text.trim()}>
          Send
        </Button>
        {done && (
          <Text size="1" tone="success">
            {done}
          </Text>
        )}
      </Row>
      {error && (
        <Text size="1" tone="danger">
          {error}
        </Text>
      )}
    </Stack>
  )
}

/// What the task is about elsewhere: a ticket, a merge request, a page. A link the fleet could
/// resolve opens in a new tab; one it could not (a merge request number in a repository the
/// orchestrator was never told about) is shown as the reference it is.
function Links({ task, onChanged }: { task: Task; onChanged: () => void }) {
  const links = task.links ?? []
  const [ref, setRef] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const path = `/api/tasks/${encodeURIComponent(task.id)}/links`
  const add = () => {
    if (!ref.trim()) return
    setBusy(true)
    setError(null)
    api
      .act(path, 'add', { ref })
      .then(() => {
        setRef('')
        onChanged()
      })
      .catch((err) => setError(message(err)))
      .finally(() => setBusy(false))
  }
  return (
    <Stack gap="3">
      {links.length === 0 ? (
        <Text size="1" tone="muted">
          Nothing linked yet.
        </Text>
      ) : (
        <Stack gap="2">
          {links.map((l: TaskLink) => {
            const url = safeUrl(l.url)
            return (
              <Row key={l.id} gap="2" justify="between">
                <Row gap="2">
                  <Badge>{LINK_KINDS[l.kind] ?? l.kind}</Badge>
                  {url ? (
                    <a href={url} target="_blank" rel="noopener noreferrer" className="fx-link fx-taskdetail__link">
                      {l.ref} <ExternalIcon />
                    </a>
                  ) : (
                    <Text size="2" mono breakAll>
                      {l.ref}
                    </Text>
                  )}
                  <Text size="1" tone="muted">
                    by {l.added_by}
                  </Text>
                </Row>
                <ActionButton
                  path={path}
                  op="remove"
                  body={{ ref: l.ref }}
                  variant="ghost"
                  onDone={onChanged}
                >
                  Remove
                </ActionButton>
              </Row>
            )
          })}
        </Stack>
      )}
      <form
        onSubmit={(e) => {
          e.preventDefault()
          add()
        }}
      >
        <Row gap="2" align="end">
          <div style={{ flexGrow: 1, minWidth: '14rem' }}>
            <TextField
              size="sm"
              aria-label="Link to add"
              placeholder="BIOINS-12, !940, #482 or a URL"
              value={ref}
              onChange={(e) => setRef(e.currentTarget.value)}
            />
          </div>
          <Button type="submit" size="sm" variant="secondary" loading={busy} disabled={!ref.trim()}>
            Link
          </Button>
        </Row>
      </form>
      {error && (
        <Text size="1" tone="danger">
          {error}
        </Text>
      )}
    </Stack>
  )
}

/// Whether an item is still waiting for the owner. An item from an older build of the API may
/// carry no state at all; it is then treated as settled rather than as a false alarm, because
/// the fleet asks again when it still needs something.
function isOpen(i: Item): boolean {
  return i.state === 'open'
}

/// One item of a task, linking to its own page, where the question and the answer are.
function ItemCard({ item, answered }: { item: Item; answered?: boolean }) {
  return (
    <Card as="div" interactive>
      <Link to={`/items/${item.id}`} className="fx-item">
        <Row gap="2">
          <Badge tone={answered ? 'neutral' : item.blocking ? 'danger' : 'warning'} dot>
            {item.kind}
          </Badge>
          <Text size="2" breakAll>
            {item.summary}
          </Text>
          {answered && (
            <Text size="1" tone="muted">
              {item.state}
            </Text>
          )}
        </Row>
      </Link>
    </Card>
  )
}

/// A transcript line. The data is whatever the agent printed — a tool result can carry a file
/// or a Slack message — so it is shown as text and never as markup.
function Event({ kind, at, data }: { kind: string; at: string; data: string }) {
  const [open, setOpen] = useState(false)
  const tone = kind === 'err' ? 'danger' : kind === 'exit' ? 'warning' : 'neutral'
  const oneLine = data.replace(/\s+/g, ' ').slice(0, 200)
  return (
    <div className="fx-event">
      <Row gap="2">
        <Badge tone={tone}>{kind}</Badge>
        <When at={at} />
        <Button size="sm" variant="ghost" onClick={() => setOpen(!open)}>
          {open ? 'less' : 'more'}
        </Button>
      </Row>
      {open ? <Pre>{data}</Pre> : <Text size="1" tone="muted" mono breakAll as="div">{oneLine}</Text>}
    </div>
  )
}

type Tab = 'sessions' | 'checklist' | 'brief' | 'summary' | 'transcript'

export default function TaskDetail() {
  const { id = '' } = useParams()
  const navigate = useNavigate()
  const state = useApi<Detail>(`/api/tasks/${encodeURIComponent(id)}`, 10_000)
  const events = useApi<Events>(`/api/tasks/${encodeURIComponent(id)}/events?limit=200`)
  const [tab, setTab] = useState<Tab>('sessions')
  const attachPath = `/api/tasks/${encodeURIComponent(id)}/attachments`
  // The edit form is part of the URL (`?edit=1`), so the board can link straight to it.
  const [params, setParams] = useSearchParams()
  const editing = params.get('edit') === '1'
  const setEditing = (on: boolean) => {
    const next = new URLSearchParams(params)
    if (on) next.set('edit', '1')
    else next.delete('edit')
    setParams(next, { replace: true })
  }
  const [saved, setSaved] = useState<string | null>(null)
  // A saved edit whose new brief could not reach the agent: saved, but worth a second look.
  const [savedWarning, setSavedWarning] = useState(false)
  const reload = () => {
    void state.reload()
    void events.reload()
  }

  return (
    <Page>
      <style href="fx-taskdetail" precedence="default">
        {css}
      </style>
      <Loaded state={state}>
        {(d) => (
          <>
            <PageHeader
              title={d.task.id}
              description={d.task.title}
              back={
                <Link to="/tasks" className="fx-link" data-muted="">
                  <ArrowLeftIcon /> All tasks
                </Link>
              }
              meta={<StatusBadge status={d.task.status} running={d.task.running} />}
              action={
                BUILTIN.includes(d.task.id) ? (
                  <Link to={`/tasks/${encodeURIComponent(d.task.id)}/session`} className="fx-link">
                    Open session
                  </Link>
                ) : (
                  <Row gap="2">
                    <Button size="sm" onClick={() => navigate(`/tasks/${encodeURIComponent(d.task.id)}/session`)}>
                      {d.task.running ? 'Watch live' : 'Open session'}
                    </Button>
                    {!editing && (
                      <Button size="sm" variant="secondary" onClick={() => setEditing(true)}>
                        Edit
                      </Button>
                    )}
                    {/* A finished task has nothing to stop; a live run of one (a follow-up) does. */}
                    {(d.task.running || !FINISHED.includes(d.task.status)) && (
                      <ActionButton
                        path={`/api/tasks/${encodeURIComponent(d.task.id)}`}
                        op="stop"
                        variant="destructive"
                        confirm={`Stop ${d.task.key}? Its agent is stopped now; a message resumes the task later.`}
                        onDone={(r) => {
                          setSaved(typeof r.done === 'string' ? r.done : null)
                          setSavedWarning(false)
                          reload()
                        }}
                      >
                        Stop
                      </ActionButton>
                    )}
                  </Row>
                )
              }
            />

            {saved && (
              <Text size="1" tone={savedWarning ? 'warning' : 'success'} as="p">
                {saved}
              </Text>
            )}

            {editing && !BUILTIN.includes(d.task.id) && (
              <EditTask
                // Not keyed on the task's `updated_at`: the page polls, and an agent at work
                // touches its task often enough to wipe a draft the owner is still writing.
                task={d.task}
                onCancel={() => setEditing(false)}
                onSaved={(done, warning) => {
                  setSaved(done)
                  setSavedWarning(warning)
                  setEditing(false)
                  reload()
                }}
              />
            )}

            <Card>
              <CardContent>
                <Facts
                  items={[
                    [
                      'Project',
                      <Link to={`/projects/${d.task.project}`} className="fx-link">
                        {d.task.project}
                      </Link>,
                    ],
                    ['Agent', d.task.agent ?? '—'],
                    ['Node', d.task.node ?? '—'],
                    [
                      'Runs with',
                      `${d.task.model ?? 'the agent’s model'}${d.task.effort ? ` · ${d.task.effort}` : ''}`,
                    ],
                    ['Created', <When at={d.task.created_at} />],
                  ]}
                />
              </CardContent>
            </Card>

            {d.task.status === 'done' && d.task.archived_at && (
              <Note tone="info" title="Archived">
                Done, and put away by the nightly archive: hidden from the board, still here. Writing to
                the task brings it back.
              </Note>
            )}

            {/* What the task produced, where it is found again later: artifact and doc links, and
                files the orchestrator keeps (an asset, a migration for devops). */}
            <Section
              title="Attachments"
              description="What this task produced: links and files, kept with it."
            >
              <Card>
                <CardContent>
                  <Stack gap="3">
                    {(d.task.attachments ?? []).length > 0 ? (
                      <AttachmentList
                        items={d.task.attachments ?? []}
                        onRemove={(a) => {
                          if (!window.confirm(`Remove ${a.ref} “${a.title}” from this task?`)) return
                          void api.act(attachPath, 'remove', { id: a.id })
                            .then(() => void state.reload())
                            .catch((e: unknown) =>
                              window.alert(`Could not remove it: ${e instanceof Error ? e.message : String(e)}`),
                            )
                        }}
                      />
                    ) : (
                      <Text size="1" tone="muted">
                        Nothing yet. Agents attach what they make with <code>fleet agent attach</code>.
                      </Text>
                    )}
                    <AttachForm path={attachPath} onDone={() => void state.reload()} />
                  </Stack>
                </CardContent>
              </Card>
            </Section>

            <Section title="Links" description="What this task is about outside the fleet.">
              <Card>
                <CardContent>
                  <Links task={d.task} onChanged={reload} />
                </CardContent>
              </Card>
            </Section>

            {!BUILTIN.includes(d.task.id) && (
              <Section
                title="Message the agent"
                description="Delivered as your message in the task’s Telegram topic is, and shown there."
              >
                <Card>
                  <CardContent>
                    <MessageBox task={d.task} onSent={reload} />
                  </CardContent>
                </Card>
              </Section>
            )}

            {/* Open items only: a task keeps its answered questions, and showing those under
                "Waiting on you" told the owner they were holding up work they had already
                answered. The rest are listed below, as a record of what was asked. */}
            {d.items.filter(isOpen).length > 0 && (
              <Section title="Waiting on you">
                <Stack gap="2">
                  {d.items.filter(isOpen).map((i) => (
                    <ItemCard key={i.id} item={i} />
                  ))}
                </Stack>
              </Section>
            )}

            {d.items.filter((i) => !isOpen(i)).length > 0 && (
              <Section title="Answered">
                <Stack gap="2">
                  {d.items
                    .filter((i) => !isOpen(i))
                    .map((i) => (
                      <ItemCard key={i.id} item={i} answered />
                    ))}
                </Stack>
              </Section>
            )}

            <Stack gap="3">
              <SegmentedControl<Tab>
                kind="tabs"
                variant="underline"
                aria-label="What to look at"
                value={tab}
                onChange={setTab}
                options={[
                  { value: 'sessions', label: 'Sessions', count: d.sessions.length },
                  { value: 'checklist', label: 'Checklist', count: d.milestones.length },
                  { value: 'brief', label: 'Brief' },
                  ...(d.task.summary ? [{ value: 'summary' as Tab, label: 'Summary' }] : []),
                  { value: 'transcript', label: 'Transcript' },
                ]}
              />

              {tab === 'sessions' && (
                <Table
                  dense
                  rows={d.sessions}
                  rowKey={(r) => r.run_id}
                  empty="This task has not run yet"
                  columns={[
                    { key: 'started', header: 'Started', cell: (r) => <When at={r.started_at} /> },
                    {
                      key: 'lasted',
                      header: 'Lasted',
                      align: 'end',
                      cell: (r) => <Duration from={r.started_at} to={r.ended_at} />,
                    },
                    {
                      key: 'node',
                      header: 'Node',
                      secondary: true,
                      cell: (r) => (
                        <Text size="1" breakAll>
                          {r.node ?? '—'}
                        </Text>
                      ),
                    },
                    {
                      key: 'session',
                      header: 'Claude session',
                      secondary: true,
                      cell: (r) => (
                        <Text size="1" mono breakAll>
                          {r.session ?? '—'}
                        </Text>
                      ),
                    },
                    {
                      key: 'ended',
                      header: 'Ended',
                      cell: (r) =>
                        r.status === 'running' ? (
                          <Badge tone="success" live>
                            live
                          </Badge>
                        ) : (
                          <Text size="1" tone={r.outcome?.startsWith('lost') ? 'warning' : 'muted'}>
                            {r.outcome ?? 'ended'}
                          </Text>
                        ),
                    },
                  ]}
                />
              )}

              {tab === 'checklist' &&
                (d.milestones.length === 0 ? (
                  <EmptyState title="No checklist" text="The agent publishes one after its plan is approved." bare />
                ) : (
                  <Card>
                    <CardContent>
                      <Stack gap="2">
                        {d.milestones.map((m) => (
                          <Row key={m.idx} gap="2" align="start">
                            <span className="fx-check" data-done={m.done || undefined} aria-hidden="true">
                              {m.done ? <CheckIcon /> : null}
                            </span>
                            <Text size="2" tone={m.done ? 'muted' : 'default'} breakAll>
                              {m.title}
                            </Text>
                          </Row>
                        ))}
                      </Stack>
                    </CardContent>
                  </Card>
                ))}

              {tab === 'brief' && <Pre>{d.task.brief ?? ''}</Pre>}

              {tab === 'summary' && <Pre>{d.task.summary ?? ''}</Pre>}

              {tab === 'transcript' && (
                <Loaded state={events}>
                  {(e) =>
                    e.events.length === 0 ? (
                      <EmptyState title="Nothing recorded" bare />
                    ) : (
                      <Card>
                        <CardContent>
                          <Stack gap="0">
                            {e.more && (
                              <Text size="1" tone="muted">
                                The latest {e.events.length} lines.
                              </Text>
                            )}
                            {e.events.map((ev) => (
                              <Event key={ev.id} kind={ev.kind} at={ev.at} data={ev.data} />
                            ))}
                          </Stack>
                        </CardContent>
                      </Card>
                    )
                  }
                </Loaded>
              )}
            </Stack>
          </>
        )}
      </Loaded>
    </Page>
  )
}

const css = /* css */ `
.fx-taskdetail__link {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  min-width: 0;
  overflow-wrap: anywhere;
}
.fx-event {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 8px 0;
  border-bottom: 1px solid var(--aui-border, #e5e5e5);
  min-width: 0;
}
.fx-event:last-child {
  border-bottom: 0;
}

/* A milestone's tick: a circle that fills in, so a finished checklist reads at a glance. */
.fx-check {
  display: grid;
  place-items: center;
  flex: none;
  width: 17px;
  height: 17px;
  margin-top: 2px;
  border: 1.5px solid var(--aui-border-strong, #d4d4d8);
  border-radius: 999px;
  color: transparent;
  font-size: 11px;
  transition:
    background-color 160ms ease,
    border-color 160ms ease,
    color 160ms ease;
}
.fx-check[data-done] {
  background: var(--aui-success, #1e8a4f);
  border-color: var(--aui-success, #1e8a4f);
  color: #fff;
}
`
