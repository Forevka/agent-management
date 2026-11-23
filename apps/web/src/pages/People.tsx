import { useMemo, useState } from 'react'
import { Link, useNavigate, useSearchParams } from 'react-router-dom'
import { MenuButton, SearchField, SectionHead } from '@/components/ui/app-shell'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { TextField } from '@/components/ui/text-field'
import { Page, PageHeader, Row, Stack, Text } from '@/components/fx/layout'
import { Table } from '@/components/fx/table'
import * as api from '../api'
import type { People as PeopleT } from '../api'
import { ErrorNote, Loaded, When, useApi } from '../ui'

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
  const state = useApi<PeopleT>(path, undefined, { keepPrevious: true })

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
      />

      <SectionHead
        title="Everyone"
        count={state.data?.total}
        right={
          <>
            <SearchField placeholder="Search people" width={220} value={q} onChange={(v) => set('q', v)} />
            <MenuButton
              label="Tag"
              value={tag || 'all'}
              onChange={(v) => set('tag', v === 'all' ? '' : v)}
              options={[
                { value: 'all', label: 'Any tag' },
                ...(state.data?.tags ?? []).map((t) => ({ value: t.tag, label: `${t.tag} (${t.count})` })),
                ...(tag && !state.data?.tags.some((t) => t.tag === tag) ? [{ value: tag, label: tag }] : []),
              ]}
            />
            <MenuButton
              label="Project"
              value={project || 'all'}
              onChange={(v) => set('project', v === 'all' ? '' : v)}
              options={[
                { value: 'all', label: 'Any project' },
                ...(state.data?.projects ?? []).map((p) => ({ value: p.project, label: `${p.project} (${p.count})` })),
                ...(project && !state.data?.projects.some((p) => p.project === project)
                  ? [{ value: project, label: project }]
                  : []),
              ]}
            />
            <MenuButton
              label="Language"
              value={language || 'all'}
              onChange={(v) => set('language', v === 'all' ? '' : v)}
              options={[
                { value: 'all', label: 'Any language' },
                ...Object.entries(LANGUAGES).map(([value, label]) => ({ value, label })),
                ...(language && !(language in LANGUAGES) ? [{ value: language, label: language }] : []),
              ]}
            />
          </>
        }
      />

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
    </Page>
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
