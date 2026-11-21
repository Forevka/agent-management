import { useMemo, useState } from 'react'
import { Link, useNavigate, useSearchParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Select } from '@/components/ui/select'
import { TextField } from '@/components/ui/text-field'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Table } from '@/components/fx/table'
import { SearchIcon } from '@/components/fx/icons'
import * as api from '../api'
import type { OwnerAccounts as OwnerAccountsT, People as PeopleT } from '../api'
import { ActionButton, ErrorNote, Loaded, When, useApi } from '../ui'

export const LANGUAGES: Record<string, string> = { en: 'English', uk: 'Ukrainian' }

/// The owner's colleagues, as the fleet has come to know them. The retrospective writes these
/// records every day from what it read, without asking; this page is where the owner reviews
/// them. The filters live in the URL, like every other list here.
export default function People() {
  const [params, setParams] = useSearchParams()
  const q = params.get('q') ?? ''
  const tag = params.get('tag') ?? ''
  const project = params.get('project') ?? ''
  const language = params.get('language') ?? ''

  const path = useMemo(() => {
    const u = new URLSearchParams()
    if (q) u.set('q', q)
    if (tag) u.set('tag', tag)
    if (project) u.set('project', project)
    if (language) u.set('language', language)
    return `/api/people?${u}`
  }, [q, tag, project, language])
  const state = useApi<PeopleT>(path)

  const set = (key: string, value: string) => {
    const next = new URLSearchParams(params)
    if (value) next.set(key, value)
    else next.delete(key)
    setParams(next, { replace: true })
  }

  return (
    <Page>
      <PageHeader
        title="People"
        description="Your colleagues the agents come into contact with. The retrospective keeps these records every day from the day’s messages and its memories, without pinging you; the orchestrator and the secretary read a person’s card before writing to them, and use it only for how they write (language, tone, form of address), never for what the fleet does. Correct anything here: a field you set stays yours, the retrospective can only add to it, and what you remove stays removed."
        meta={
          state.data && (
            <Text size="1" tone="muted">
              {state.data.people.length} of {state.data.total}
            </Text>
          )
        }
      />

      <Row gap="2" align="end">
        <div style={{ minWidth: '15rem', flexGrow: 1 }}>
          <TextField
            placeholder="Search a name, handle, tag or project"
            defaultValue={q}
            onChange={(e) => set('q', e.currentTarget.value)}
            prefix={<SearchIcon />}
            clearable
            onClear={() => set('q', '')}
            size="sm"
            aria-label="Search people"
          />
        </div>
        <div style={{ minWidth: '10rem' }}>
          <Select
            size="sm"
            aria-label="Tag"
            value={tag || 'all'}
            onChange={(v) => set('tag', v === 'all' ? '' : v)}
            options={[
              { value: 'all', label: 'Any tag' },
              ...(state.data?.tags ?? []).map((t) => ({ value: t.tag, label: `${t.tag} (${t.count})` })),
            ]}
          />
        </div>
        <div style={{ minWidth: '10rem' }}>
          <Select
            size="sm"
            aria-label="Project"
            value={project || 'all'}
            onChange={(v) => set('project', v === 'all' ? '' : v)}
            options={[
              { value: 'all', label: 'Any project' },
              ...(state.data?.projects ?? []).map((p) => ({
                value: p.project,
                label: `${p.project} (${p.count})`,
              })),
            ]}
          />
        </div>
        <div style={{ minWidth: '9rem' }}>
          <Select
            size="sm"
            aria-label="Language"
            value={language || 'all'}
            onChange={(v) => set('language', v === 'all' ? '' : v)}
            options={[
              { value: 'all', label: 'Any language' },
              ...Object.entries(LANGUAGES).map(([value, label]) => ({ value, label })),
            ]}
          />
        </div>
      </Row>

      <Loaded state={state}>
        {(d) => (
          <Table
            dense
            rows={d.people}
            rowKey={(p) => String(p.id)}
            loading={state.loading}
            empty={
              q || tag || project || language
                ? 'Nobody matches those filters'
                : 'No records yet: the retrospective adds people as the fleet meets them'
            }
            columns={[
              {
                key: 'name',
                header: 'Name',
                width: '22%',
                cell: (p) => (
                  <Stack gap="1">
                    <Link to={`/people/${p.id}`} className="fx-link">
                      {p.name}
                    </Link>
                    {p.owner_edited_at && (
                      <Text size="1" tone="muted">
                        edited by you
                      </Text>
                    )}
                  </Stack>
                ),
              },
              {
                key: 'position',
                header: 'Position',
                cell: (p) => (
                  <Text size="1" breakAll>
                    {p.position ?? '—'}
                  </Text>
                ),
              },
              {
                key: 'projects',
                header: 'Projects',
                secondary: true,
                cell: (p) => <Chips items={p.projects} onPick={(v) => set('project', v)} />,
              },
              {
                key: 'language',
                header: 'Language',
                secondary: true,
                cell: (p) =>
                  p.language ? (
                    <Badge>{LANGUAGES[p.language] ?? p.language}</Badge>
                  ) : (
                    <Text size="1" tone="muted">
                      —
                    </Text>
                  ),
              },
              {
                key: 'handles',
                header: 'Reach',
                secondary: true,
                cell: (p) => (
                  <Stack gap="1">
                    {p.contacts.length === 0 && (
                      <Text size="1" tone="muted">
                        —
                      </Text>
                    )}
                    {p.contacts.map((c) => (
                      <Text key={c.id} size="1" breakAll>
                        <Text size="1" tone="muted">
                          {c.source}
                        </Text>{' '}
                        {c.handle}
                      </Text>
                    ))}
                  </Stack>
                ),
              },
              {
                key: 'tags',
                header: 'Tags',
                secondary: true,
                cell: (p) => <Chips items={p.tags} onPick={(v) => set('tag', v)} />,
              },
              {
                key: 'last',
                header: 'Last in touch',
                align: 'end',
                cell: (p) => <When at={p.last_interaction} />,
              },
            ]}
          />
        )}
      </Loaded>

      <AddPerson />

      <OwnAccounts onChanged={() => void state.reload()} />
    </Page>
  )
}

const WHY: Record<string, string> = {
  flagged: 'a source marked its messages as yours',
  direct: 'it wrote in two of your direct messages',
  name: 'it goes by the name your own messages carry',
}

const SOURCE_NAMES: Record<string, string> = { slack: 'Slack', gchat: 'Google Chat' }

/// The owner's own accounts. A source can only tell the owner's messages apart when it knows
/// their id there, and Google Chat often does not, so the owner names them here once.
function OwnAccounts({ onChanged }: { onChanged: () => void }) {
  const state = useApi<OwnerAccountsT>('/api/owner-accounts')
  // What the last action did (which records it took the account off or deleted), as the server
  // said it.
  const [said, setSaid] = useState<string | null>(null)
  const changed = (r?: { done?: unknown }) => {
    setSaid(typeof r?.done === 'string' ? r.done : null)
    void state.reload()
    onChanged()
  }
  return (
    <Section
      title="Your own accounts"
      description="The accounts you write from on each source. Their messages count as yours (never as a colleague’s), Google Chat learns which user is you (for mentions too), and they never become one of the people. Naming one takes it off any record, and deletes a record left with nothing else unless you edited it. “Not mine” is remembered: that account’s messages stay a colleague’s even if a source marks them as yours. Sources other than Slack and Google Chat are kept for when a feed reads them; today they only keep the account off people’s records."
    >
      <Loaded state={state}>
        {(d) => (
          <Stack gap="3">
            {said && (
              <Text size="1" tone="muted">
                {said}
              </Text>
            )}
            <Table
              dense
              rows={d.accounts}
              rowKey={(a) => `${a.source}|${a.native_id}`}
              empty="None named yet"
              columns={[
                {
                  key: 'source',
                  header: 'Source',
                  width: '14%',
                  cell: (a) => <Badge>{SOURCE_NAMES[a.source] ?? a.source}</Badge>,
                },
                {
                  key: 'id',
                  header: 'User id',
                  cell: (a) => (
                    <Text size="1" breakAll>
                      {a.native_id}
                    </Text>
                  ),
                },
                {
                  key: 'name',
                  header: 'Shows as',
                  secondary: true,
                  cell: (a) => (
                    <Text size="1" breakAll>
                      {a.label ?? a.name ?? '—'}
                    </Text>
                  ),
                },
                {
                  key: 'messages',
                  header: 'Messages',
                  secondary: true,
                  align: 'end',
                  cell: (a) => <Text size="1">{a.messages}</Text>,
                },
                {
                  key: 'remove',
                  header: '',
                  align: 'end',
                  cell: (a) => (
                    <ActionButton
                      op="remove"
                      path="/api/owner-accounts"
                      body={{ source: a.source, native_id: a.native_id }}
                      variant="ghost"
                      confirm={`${a.source} ${a.native_id} is not yours? Its messages are no longer marked as yours, even where a source marks them so, its recent ones go to the secretary, and it may show up among the people again. A record deleted when you named it does not come back.`}
                      onDone={changed}
                    >
                      Not mine
                    </ActionButton>
                  ),
                },
              ]}
            />
            {d.suggestions.length > 0 && (
              <Stack gap="2">
                <Text size="1" tone="muted">
                  These look like yours:
                </Text>
                {d.suggestions.map((s) => (
                  <Row key={`${s.source}|${s.native_id}`} gap="2" align="center">
                    <Badge>{SOURCE_NAMES[s.source] ?? s.source}</Badge>
                    <Text size="1" breakAll>
                      {s.name ?? s.native_id}{' '}
                      <Text size="1" tone="muted">
                        ({s.native_id}, {s.messages} message(s); {s.why.map((w) => WHY[w] ?? w).join('; ')})
                      </Text>
                    </Text>
                    {s.person && (
                      <Link to={`/people/${s.person.id}`} className="fx-link">
                        on #{s.person.id} {s.person.name}
                      </Link>
                    )}
                    <ActionButton
                      op="add"
                      path="/api/owner-accounts"
                      body={{ source: s.source, native_id: s.native_id }}
                      confirm={[
                        `Is ${s.source} ${s.native_id}${s.name ? ` (${s.name})` : ''} you?`,
                        `Its ${s.messages} message(s) become yours and are no longer triaged by the secretary.`,
                        s.source === 'gchat' ? 'Google Chat will be told this user is you.' : '',
                        s.person
                          ? `It is taken off #${s.person.id} ${s.person.name}, whose record is deleted if nothing else is left of it and you never edited it.`
                          : '',
                      ]
                        .filter(Boolean)
                        .join(' ')}
                      onDone={changed}
                    >
                      This is me
                    </ActionButton>
                    <ActionButton
                      op="remove"
                      path="/api/owner-accounts"
                      body={{ source: s.source, native_id: s.native_id }}
                      variant="ghost"
                      confirm={`${s.source} ${s.native_id} is not yours? It is no longer suggested, and its messages count as a colleague’s even if a source marks them as yours.`}
                      onDone={changed}
                    >
                      Not mine
                    </ActionButton>
                  </Row>
                ))}
              </Stack>
            )}
            <AddOwnAccount onDone={changed} />
          </Stack>
        )}
      </Loaded>
    </Section>
  )
}

/// An account named by hand: Slack and Google Chat, or a source no feed reads yet (GitLab, Jira).
function AddOwnAccount({ onDone }: { onDone: () => void }) {
  const [source, setSource] = useState('gchat')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [done, setDone] = useState<string | null>(null)
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault()
        const form = e.currentTarget
        const data = new FormData(form)
        const field = (k: string) => String(data.get(k) ?? '').trim()
        const src = source === 'other' ? field('other') : source
        const native_id = field('native_id')
        if (!src || !native_id) {
          setError(!src ? 'Name the source' : 'Your user id there is needed')
          return
        }
        setBusy(true)
        setError(null)
        setDone(null)
        api
          .act<{ done: string }>('/api/owner-accounts', 'add', { source: src, native_id, label: field('label') })
          .then((r) => {
            setDone(r.done)
            form.reset()
            onDone()
          })
          .catch((err) => setError(err instanceof Error ? err.message : String(err)))
          .finally(() => setBusy(false))
      }}
    >
      <Stack gap="2">
        <Row gap="2" align="end">
          <div style={{ minWidth: '9rem' }}>
            <Select
              size="sm"
              label="Source"
              value={source}
              onChange={(v) => setSource(v ?? 'gchat')}
              options={[
                { value: 'gchat', label: 'Google Chat' },
                { value: 'slack', label: 'Slack' },
                { value: 'other', label: 'Other…' },
              ]}
            />
          </div>
          {source === 'other' && (
            <div style={{ minWidth: '8rem' }}>
              <TextField name="other" label="Source name" placeholder="gitlab" size="sm" />
            </div>
          )}
          <div style={{ flexGrow: 1, minWidth: '12rem' }}>
            <TextField
              name="native_id"
              label="Your user id there"
              placeholder={source === 'slack' ? 'U01ABCDEF' : source === 'gchat' ? 'users/1234567890' : 'username'}
              size="sm"
            />
          </div>
          <div style={{ minWidth: '10rem' }}>
            <TextField name="label" label="Shows as (optional)" placeholder="@you" size="sm" />
          </div>
          <Button type="submit" variant="secondary" loading={busy}>
            Add
          </Button>
        </Row>
        {error && <ErrorNote>{error}</ErrorNote>}
        {done && (
          <Text size="1" tone="muted">
            {done}
          </Text>
        )}
      </Stack>
    </form>
  )
}

/// Short lists as badges; a click filters by that value.
function Chips({ items, onPick }: { items: string[]; onPick: (v: string) => void }) {
  if (items.length === 0) {
    return (
      <Text size="1" tone="muted">
        —
      </Text>
    )
  }
  return (
    <Row gap="1">
      {items.map((t) => (
        <button key={t} type="button" className="fx-chip" onClick={() => onPick(t)} title={`Only ${t}`}>
          <Badge>{t}</Badge>
        </button>
      ))}
      <style href="fx-chip" precedence="default">
        {chipCss}
      </style>
    </Row>
  )
}

const chipCss = /* css */ `
.fx-chip {
  all: unset;
  cursor: pointer;
  border-radius: var(--aui-radius-sm, 7px);
}
.fx-chip:focus-visible {
  outline: 2px solid var(--aui-focus, rgba(0, 96, 226, 0.7));
  outline-offset: 2px;
}
`

/// Someone the retrospective has not met yet: a name is enough, the rest is filled in on their page.
function AddPerson() {
  const navigate = useNavigate()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault()
        const name = String(new FormData(e.currentTarget).get('name') ?? '').trim()
        if (!name) return
        setBusy(true)
        setError(null)
        api
          .act<{ id: number }>('/api/people', 'create', { name })
          .then((r) => navigate(`/people/${r.id}`))
          .catch((err) => setError(err instanceof Error ? err.message : String(err)))
          .finally(() => setBusy(false))
      }}
    >
      <Stack gap="2">
        <Row gap="2" align="end">
          <div style={{ flexGrow: 1, minWidth: '14rem' }}>
            <TextField name="name" label="Add someone" placeholder="Their name" size="sm" />
          </div>
          <Button type="submit" variant="secondary" loading={busy}>
            Add
          </Button>
        </Row>
        {error && <ErrorNote>{error}</ErrorNote>}
      </Stack>
    </form>
  )
}
