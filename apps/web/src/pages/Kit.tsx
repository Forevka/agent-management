import { useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardFooter, CardHeader } from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import { Select } from '@/components/ui/select'
import { Switch } from '@/components/ui/switch'
import { TextArea } from '@/components/ui/text-area'
import { TextField } from '@/components/ui/text-field'
import { Grid, Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Note } from '@/components/fx/note'
import { Table } from '@/components/fx/table'
import * as api from '../api'
import type { Kit as KitT, KitCapture, KitItem, KitKind, KitState } from '../api'
import { ActionButton, ErrorNote, Loaded, When, useApi } from '../ui'

const PATH = '/api/kit'

const KINDS: { kind: KitKind; title: string; description: string }[] = [
  {
    kind: 'skill',
    title: 'Skills',
    description: 'Written into each machine’s Claude config as skills/<name>/, beside the owner’s own.',
  },
  {
    kind: 'plugin',
    title: 'Plugins',
    description:
      'Installed at user scope with claude plugin install, from their marketplace. A plugin’s own MCP servers do not load in agent sessions: add those below if agents need them.',
  },
  {
    kind: 'mcp',
    title: 'MCP servers',
    description:
      'Installed at user scope for your own Claude Code, and the only servers an agent session gets besides the fleet’s own.',
  },
]

const STATE_TONE: Record<KitState, 'neutral' | 'success' | 'danger' | 'warning' | 'info'> = {
  pending: 'info',
  installed: 'success',
  failed: 'danger',
  conflict: 'warning',
  removed: 'neutral',
}

/// The kit: the skills, plugins and MCP servers every machine in the fleet gets, for the owner's
/// own Claude Code and for the agents. Only the owner changes it, here.
export default function Kit() {
  const state = useApi<KitT>(PATH, 15000)
  const reload = () => void state.reload()

  return (
    <Page>
      <PageHeader
        title="Kit"
        description="The skills, plugins and MCP servers every node installs, for your own Claude Code and for the agents. An agent session gets exactly the enabled MCP servers below, plus the fleet’s own: nothing else configured on a machine. Each server is a process per session, so keep the list short."
      />
      <Loaded state={state}>
        {(d) => (
          <>
            <ImportSection kit={d} onImported={reload} />
            {KINDS.map((k) => (
              <ItemsSection
                key={k.kind}
                title={k.title}
                description={k.description}
                items={d.items.filter((i) => i.kind === k.kind)}
                onChange={reload}
              />
            ))}
            <AddSection onAdded={reload} />
            <SecretsSection keys={d.secrets} onChange={reload} />
            <MatrixSection kit={d} />
          </>
        )}
      </Loaded>
    </Page>
  )
}

function ItemsSection({
  title,
  description,
  items,
  onChange,
}: {
  title: string
  description: string
  items: KitItem[]
  onChange: () => void
}) {
  return (
    <Section title={title} description={description}>
      <Table
        rows={items}
        rowKey={(i) => String(i.id)}
        empty="None in the kit"
        columns={[
          {
            key: 'name',
            header: 'Name',
            cell: (i) =>
              i.kind === 'plugin' ? (
                <Stack gap="1">
                  <Text mono size="2">
                    {i.name}
                  </Text>
                  <Text size="1" tone="muted" mono breakAll>
                    {String(i.spec.marketplace_source ?? '')}
                  </Text>
                </Stack>
              ) : (
                <Link to={`/kit/${i.id}`} className="fx-link">
                  <Text mono size="2">
                    {i.name}
                  </Text>
                </Link>
              ),
          },
          {
            key: 'what',
            header: '',
            secondary: true,
            cell: (i) => (
              <Row gap="2">
                {i.files && (
                  <Text size="1" tone="muted">
                    {i.files.length} file{i.files.length === 1 ? '' : 's'}
                  </Text>
                )}
                {i.missing_secrets.length > 0 && (
                  <Badge tone="danger" title={i.missing_secrets.join(', ')}>
                    secret not set
                  </Badge>
                )}
                <When at={i.updated_at} />
              </Row>
            ),
          },
          {
            key: 'enabled',
            header: 'On',
            width: '7ch',
            cell: (i) => <EnabledSwitch item={i} onChange={onChange} />,
          },
          {
            key: 'act',
            header: '',
            align: 'end',
            cell: (i) => (
              <ActionButton
                path={PATH}
                op="delete"
                body={{ id: i.id }}
                variant="ghost"
                confirm={`Delete ${i.name} from the kit? The nodes remove what the kit installed of it.`}
                onDone={onChange}
              >
                Delete
              </ActionButton>
            ),
          },
        ]}
      />
    </Section>
  )
}

function EnabledSwitch({ item, onChange }: { item: KitItem; onChange: () => void }) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  return (
    <span title={error ?? undefined}>
      <Switch
        size="sm"
        checked={item.enabled}
        disabled={busy}
        aria-label={`${item.name} on every node`}
        onChange={async (e) => {
          setBusy(true)
          setError(null)
          try {
            await api.act(PATH, e.currentTarget.checked ? 'enable' : 'disable', { id: item.id })
            onChange()
          } catch (err) {
            setError(err instanceof Error ? err.message : String(err))
          } finally {
            setBusy(false)
          }
        }}
      />
    </span>
  )
}

/// Asks one node what it has, then imports what the owner ticks. Secrets never reach this page:
/// the capture stays on the orchestrator, which moves each value into the vault on import.
function ImportSection({ kit, onImported }: { kit: KitT; onImported: () => void }) {
  const [params, setParams] = useSearchParams()
  const node = params.get('node')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [capture, setCapture] = useState<KitCapture | null>(null)
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const [done, setDone] = useState<string | null>(null)
  const usable = kit.nodes.filter((n) => n.online && n.current)

  const ask = async () => {
    if (!node) return
    setBusy(true)
    setError(null)
    setDone(null)
    try {
      const c = await api.act<KitCapture>(PATH, 'capture', { node })
      setCapture(c)
      // What is not in the kit yet is ticked; what is, is offered for replacing.
      setPicked(
        new Set([
          ...c.skills.filter((s) => !s.in_kit).map((s) => `skill:${s.name}`),
          ...c.plugins.filter((p) => !p.in_kit).map((p) => `plugin:${p.name}`),
          ...c.mcp.filter((m) => !m.in_kit).map((m) => `mcp:${m.name}`),
        ]),
      )
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const importPicked = async () => {
    if (!capture) return
    setBusy(true)
    setError(null)
    const of = (kind: string) =>
      [...picked].filter((p) => p.startsWith(`${kind}:`)).map((p) => p.slice(kind.length + 1))
    try {
      const r = await api.act<{ done: string }>(PATH, 'import', {
        capture: capture.capture,
        skills: of('skill'),
        plugins: of('plugin'),
        mcp: of('mcp'),
      })
      setDone(r.done)
      setCapture(null)
      onImported()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const toggle = (key: string, on: boolean) =>
    setPicked((p) => {
      const next = new Set(p)
      if (on) next.add(key)
      else next.delete(key)
      return next
    })

  const row = (key: string, name: string, inKit: boolean, detail: React.ReactNode) => (
    <Checkbox
      key={key}
      variant="card"
      checked={picked.has(key)}
      onChange={(e) => toggle(key, e.currentTarget.checked)}
      label={
        <Row gap="2">
          <Text mono size="2">
            {name}
          </Text>
          {inKit && <Badge tone="info">in the kit: importing replaces it</Badge>}
        </Row>
      }
      description={detail}
    />
  )

  return (
    <Section
      title="Import from a node"
      description="Reads the skills, plugins and user-scope MCP servers a machine has. Claude.ai connectors belong to the account, not a machine, so they are not offered. An MCP server's env and header values, and URL parameters and flags named like a secret (access_token, --api-key), go into the vault; any other secret in a config, replace with ${secret:NAME} on the server's page."
    >
      <Card>
        <CardContent>
          <Stack gap="3">
            <Row gap="3" align="end">
              <div style={{ minWidth: '16rem' }}>
                <Select
                  label="Node"
                  size="sm"
                  placeholder={usable.length ? 'Choose a node…' : 'No node is online and up to date'}
                  value={node}
                  options={usable.map((n) => ({ value: n.id, label: n.name, description: n.version }))}
                  onChange={(v) => {
                    setParams({ node: v }, { replace: true })
                    setCapture(null)
                  }}
                />
              </div>
              <Button variant="secondary" size="sm" onClick={() => void ask()} disabled={!node} loading={busy && !capture}>
                See what it has
              </Button>
            </Row>
            {error && <ErrorNote>{error}</ErrorNote>}
            {done && (
              <Note tone="success" title="Imported">
                {done}
              </Note>
            )}
            {capture && (
              <Stack gap="3">
                {capture.skills.length + capture.plugins.length + capture.mcp.length === 0 && (
                  <Text tone="muted">It has nothing the kit can take.</Text>
                )}
                {capture.skills.length > 0 && (
                  <Stack gap="2">
                    <Text weight="medium">Skills</Text>
                    {capture.skills.map((s) =>
                      row(
                        `skill:${s.name}`,
                        s.name,
                        s.in_kit,
                        s.files.map((f) => f.path).join(', '),
                      ),
                    )}
                  </Stack>
                )}
                {capture.plugins.length > 0 && (
                  <Stack gap="2">
                    <Text weight="medium">Plugins</Text>
                    {capture.plugins.map((p) => row(`plugin:${p.name}`, p.name, p.in_kit, p.marketplace_source))}
                  </Stack>
                )}
                {capture.mcp.length > 0 && (
                  <Stack gap="2">
                    <Text weight="medium">MCP servers</Text>
                    {capture.mcp.map((m) =>
                      row(
                        `mcp:${m.name}`,
                        m.name,
                        m.in_kit,
                        m.secrets.length > 0
                          ? `Its values for ${m.secrets.join(', ')} go into the vault; the kit keeps placeholders.`
                          : 'No env or header values.',
                      ),
                    )}
                  </Stack>
                )}
                {capture.skipped.length > 0 && (
                  <Note tone="warning" title="Left out">
                    <Stack gap="1">
                      {capture.skipped.map((s, i) => (
                        <Text key={i} size="1" breakAll>
                          {s.kind} <Text mono size="1">{s.name}</Text>: {s.reason}
                        </Text>
                      ))}
                    </Stack>
                  </Note>
                )}
              </Stack>
            )}
          </Stack>
        </CardContent>
        {capture && (
          <CardFooter>
            <Row gap="2">
              <Button size="sm" onClick={() => void importPicked()} loading={busy} disabled={picked.size === 0}>
                Import {picked.size} selected
              </Button>
              <Button size="sm" variant="ghost" onClick={() => setCapture(null)}>
                Cancel
              </Button>
            </Row>
          </CardFooter>
        )}
      </Card>
    </Section>
  )
}

/// A form that runs one kit action and resets itself once it worked.
function AddForm({
  title,
  description,
  op,
  build,
  onDone,
  children,
}: {
  title: string
  description: string
  op: string
  build: (f: FormData) => unknown
  onDone: () => void
  children: React.ReactNode
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  return (
    <Card>
      <CardHeader title={title} description={description} />
      <CardContent>
        <form
          onSubmit={async (e) => {
            e.preventDefault()
            const form = e.currentTarget
            setBusy(true)
            setError(null)
            try {
              await api.act(PATH, op, build(new FormData(form)))
              form.reset()
              onDone()
            } catch (err) {
              setError(err instanceof Error ? err.message : String(err))
            } finally {
              setBusy(false)
            }
          }}
        >
          <Stack gap="3">
            {children}
            {error && (
              <Text size="1" tone="danger">
                {error}
              </Text>
            )}
            <Row>
              <Button type="submit" variant="secondary" size="sm" loading={busy}>
                Add
              </Button>
            </Row>
          </Stack>
        </form>
      </CardContent>
    </Card>
  )
}

function AddSection({ onAdded }: { onAdded: () => void }) {
  return (
    <Section title="Add">
      <Grid min={300}>
        <AddForm
          title="A plugin"
          description="From a marketplace: owner/repo, a git URL or a path, as claude plugin marketplace add takes it."
          op="plugin_add"
          build={(f) => ({ source: f.get('source'), marketplace: f.get('marketplace'), plugin: f.get('plugin') })}
          onDone={onAdded}
        >
          <TextField name="source" label="Marketplace source" placeholder="anthropics/claude-plugins-official" required size="sm" />
          <TextField name="marketplace" label="Marketplace name" placeholder="claude-plugins-official" required size="sm" />
          <TextField name="plugin" label="Plugin" placeholder="commit-commands" required size="sm" />
        </AddForm>
        <AddForm
          title="A skill"
          description="Its SKILL.md; add more files on its page."
          op="skill_save"
          build={(f) => ({ name: f.get('name'), files: { 'SKILL.md': f.get('skill') } })}
          onDone={onAdded}
        >
          <TextField name="name" label="Name" placeholder="release-notes" required size="sm" />
          <TextArea name="skill" label="SKILL.md" minRows={4} maxRows={12} required placeholder={'---\nname: release-notes\ndescription: …\n---\n'} />
        </AddForm>
        <AddForm
          title="An MCP server"
          description="Its config as JSON. Env and header values move into the vault; write ${secret:NAME} to use one already there."
          op="mcp_save"
          build={(f) => ({ name: f.get('name'), config: f.get('config') })}
          onDone={onAdded}
        >
          <TextField name="name" label="Name" placeholder="sentry" required size="sm" />
          <TextArea
            name="config"
            label="Config"
            minRows={4}
            maxRows={12}
            required
            placeholder={'{"type": "http", "url": "https://mcp.sentry.dev/mcp"}'}
          />
        </AddForm>
      </Grid>
    </Section>
  )
}

/// The kit's MCP secrets, by name. Write-only: a value is never read back.
function SecretsSection({ keys, onChange }: { keys: string[]; onChange: () => void }) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  return (
    <Section
      title="Secrets"
      description="What the kit's MCP servers use through ${secret:NAME}. Kept encrypted in the kit secret folder; values go only to the nodes."
    >
      <Card>
        <CardContent>
          <Stack gap="3">
            <Row gap="2">
              {keys.length === 0 ? (
                <Text tone="muted" size="1">
                  None yet
                </Text>
              ) : (
                keys.map((k) => (
                  <Badge key={k}>
                    <Text mono size="1">
                      {k}
                    </Text>
                  </Badge>
                ))
              )}
            </Row>
            <form
              onSubmit={async (e) => {
                e.preventDefault()
                const form = e.currentTarget
                const f = new FormData(form)
                setBusy(true)
                setError(null)
                try {
                  await api.act(PATH, 'secret_set', { key: f.get('key'), value: f.get('value') })
                  // The value must not stay sitting in the page after it is stored.
                  form.reset()
                  onChange()
                } catch (err) {
                  setError(err instanceof Error ? err.message : String(err))
                } finally {
                  setBusy(false)
                }
              }}
            >
              <Row gap="3" align="end">
                <div style={{ minWidth: '13rem' }}>
                  <TextField name="key" label="Name" list="kit-secrets" required size="sm" placeholder="SENTRY_TOKEN" />
                  <datalist id="kit-secrets">
                    {keys.map((k) => (
                      <option key={k} value={k} />
                    ))}
                  </datalist>
                </div>
                <div style={{ minWidth: '16rem', flexGrow: 1 }}>
                  <TextField name="value" label="Value" type="password" autoComplete="off" required size="sm" />
                </div>
                <Button type="submit" variant="secondary" size="sm" loading={busy}>
                  Set
                </Button>
              </Row>
              {error && (
                <Text size="1" tone="danger">
                  {error}
                </Text>
              )}
            </form>
          </Stack>
        </CardContent>
      </Card>
    </Section>
  )
}

/// Every node against every item: what each machine has, and why not when it does not.
function MatrixSection({ kit }: { kit: KitT }) {
  const items = kit.items.filter((i) => i.enabled || kit.status.some((s) => s.item === i.id))
  const cell = (node: string, item: number) => kit.status.find((s) => s.node === node && s.item === item)
  return (
    <Section title="On the nodes" description="Hover a state for its reason. A node gets the kit once it runs this release.">
      <Table
        rows={items}
        rowKey={(i) => String(i.id)}
        empty="Nothing to install"
        columns={[
          {
            key: 'item',
            header: 'Item',
            cell: (i) => (
              <Row gap="2">
                <Badge>{i.kind}</Badge>
                <Text mono size="2">
                  {i.name}
                </Text>
              </Row>
            ),
          },
          ...kit.nodes.map((n) => ({
            key: n.id,
            header: (
              <span title={n.current ? n.version : `${n.version}: gets the kit once it updates`}>
                {n.name}
                {!n.online && ' (offline)'}
              </span>
            ),
            cell: (i: KitItem) => {
              const s = cell(n.id, i.id)
              if (!s) return <Text tone="muted">—</Text>
              return (
                <Badge tone={STATE_TONE[s.state]} title={s.detail || undefined}>
                  {s.state}
                </Badge>
              )
            },
          })),
        ]}
      />
    </Section>
  )
}
