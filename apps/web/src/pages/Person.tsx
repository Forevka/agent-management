import { useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { Select } from '@/components/ui/select'
import { TagInput } from '@/components/ui/tag-input'
import { TextArea } from '@/components/ui/text-area'
import { TextField } from '@/components/ui/text-field'
import { Grid, Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Facts } from '@/components/fx/table'
import { ArrowLeftIcon } from '@/components/fx/icons'
import * as api from '../api'
import type { Person as PersonT, PersonDetail } from '../api'
import { ActionButton, ErrorNote, Loaded, When, useApi } from '../ui'
import { LANGUAGES } from './People'

const WHO: Record<string, string> = { retro: 'the retrospective', owner: 'you' }

/// What the server keeps a tag as (see `people::tag`), so the field shows it the same way.
const kebab = (t: string) =>
  t
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}]+/gu, '-')
    .replace(/^-+|-+$/g, '')

/// One colleague: everything the fleet knows about them, what was done to the record and by
/// whom, and the owner's edit. Every text here may have been written from a third party's
/// message, so it is shown as text and nothing else.
export default function Person() {
  const { id = '' } = useParams()
  const path = `/api/people/${encodeURIComponent(id)}`
  const state = useApi<PersonDetail>(path)
  const navigate = useNavigate()

  return (
    <Page>
      <Loaded state={state}>
        {(d) => (
          <>
            <PageHeader
              title={d.person.name}
              description={d.person.position ?? undefined}
              back={
                <Link to="/people" className="fx-link" data-muted="">
                  <ArrowLeftIcon /> Everyone
                </Link>
              }
              meta={
                <Row gap="2">
                  <Text size="1" tone="muted" mono>
                    #{d.person.id}
                  </Text>
                  <Badge tone={d.person.updated_by === 'owner' ? 'info' : 'neutral'}>
                    updated by {WHO[d.person.updated_by] ?? d.person.updated_by}
                  </Badge>
                  <When at={d.person.updated_at} />
                </Row>
              }
            />

            <Card>
              <CardContent>
                <Facts
                  items={[
                    ['Language', d.person.language ? (LANGUAGES[d.person.language] ?? d.person.language) : '—'],
                    ['Projects', d.person.projects.length ? d.person.projects.join(', ') : '—'],
                    ['Last in touch', <When key="last" at={d.person.last_interaction} />],
                    [
                      'Reach',
                      d.person.contacts.length ? (
                        <Stack key="reach" gap="1">
                          {d.person.contacts.map((c) => (
                            <Text key={c.id} size="2" breakAll>
                              <Text size="2" tone="muted">
                                {c.source}
                              </Text>{' '}
                              {c.handle}
                              {c.native_id && c.native_id !== c.handle && (
                                <Text size="1" tone="muted" mono>
                                  {' '}
                                  {c.native_id}
                                </Text>
                              )}
                            </Text>
                          ))}
                        </Stack>
                      ) : (
                        '—'
                      ),
                    ],
                    ['In the records since', <When key="since" at={d.person.created_at} />],
                  ]}
                />
              </CardContent>
            </Card>

            <Grid min={300} gap="3">
              <Card>
                <CardHeader title="What they are involved in" />
                <CardContent>
                  <Text as="p" size="2" breakAll tone={d.person.brief ? 'default' : 'muted'}>
                    {d.person.brief ?? 'Nothing recorded yet.'}
                  </Text>
                </CardContent>
              </Card>
              <Card>
                <CardHeader title="Observations" />
                <CardContent>
                  {d.person.observations.length === 0 ? (
                    <Text size="2" tone="muted">
                      None yet.
                    </Text>
                  ) : (
                    <Stack gap="1" role="list">
                      {d.person.observations.map((o) => (
                        <Text key={o} role="listitem" size="2" breakAll>
                          {o}
                        </Text>
                      ))}
                    </Stack>
                  )}
                </CardContent>
              </Card>
            </Grid>

            {d.person.tags.length > 0 && (
              <Section title="Tags" description="What fits no field yet. A tag that keeps coming back is worth a field of its own.">
                <Row gap="1">
                  {d.person.tags.map((t) => (
                    <Link key={t} to={`/people?tag=${encodeURIComponent(t)}`} className="fx-link" data-muted="">
                      <Badge>{t}</Badge>
                    </Link>
                  ))}
                </Row>
              </Section>
            )}

            <Section title="Edit">
              {/* Keyed by the last update, so a save (or the retrospective's next pass) resets it. */}
              <EditForm
                key={d.person.updated_at}
                person={d.person}
                sources={d.sources}
                path={path}
                onSaved={() => void state.reload()}
              />
            </Section>

            <Section title="History">
              {d.history.length === 0 ? (
                <EmptyState title="Nothing recorded" bare />
              ) : (
                <Stack gap="1">
                  {d.history.map((h, i) => (
                    <Row key={i} gap="2">
                      <When at={h.at} />
                      <Badge tone={h.actor === 'owner' ? 'info' : 'neutral'}>{WHO[h.actor] ?? h.actor}</Badge>
                      <Text size="1" tone="muted">
                        {h.action.replace('person.', '')}
                        {h.changed && h.changed.length > 0 ? `: ${h.changed.join(', ')}` : ''}
                      </Text>
                    </Row>
                  ))}
                </Stack>
              )}
            </Section>

            <Row>
              <ActionButton
                op="delete"
                path={path}
                variant="destructive"
                confirm={`Delete ${d.person.name}'s record? The retrospective may create a new one the next time they appear.`}
                onDone={() => navigate('/people')}
              >
                Delete record
              </ActionButton>
            </Row>
          </>
        )}
      </Loaded>
    </Page>
  )
}

type ContactDraft = { source: string; handle: string; native_id: string }

/// The owner's edit: every field, the lists whole. Saving makes the record theirs, which the
/// retrospective is told to respect.
function EditForm({
  person,
  sources,
  path,
  onSaved,
}: {
  person: PersonT
  sources: string[]
  path: string
  onSaved: () => void
}) {
  const [language, setLanguage] = useState<string>(person.language ?? '')
  const [projects, setProjects] = useState<string[]>(person.projects)
  const [tags, setTags] = useState<string[]>(person.tags)
  const [contacts, setContacts] = useState<ContactDraft[]>(
    person.contacts.map((c) => ({ source: c.source, handle: c.handle, native_id: c.native_id ?? '' })),
  )
  const [draft, setDraft] = useState<ContactDraft>({ source: sources[0] ?? 'slack', handle: '', native_id: '' })
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const addContact = () => {
    if (!draft.handle.trim()) return
    setContacts([...contacts, { ...draft, handle: draft.handle.trim(), native_id: draft.native_id.trim() }])
    setDraft({ ...draft, handle: '', native_id: '' })
  }

  return (
    <Card>
      <CardContent>
        <form
          onSubmit={(e) => {
            e.preventDefault()
            const f = new FormData(e.currentTarget)
            setSaving(true)
            setError(null)
            api
              .act(path, 'edit', {
                name: f.get('name'),
                position: f.get('position'),
                brief: f.get('brief'),
                language,
                projects,
                tags,
                // One observation per line.
                observations: String(f.get('observations') ?? '')
                  .split('\n')
                  .map((l) => l.trim())
                  .filter(Boolean),
                contacts: contacts.map((c) => ({
                  source: c.source,
                  handle: c.handle,
                  native_id: c.native_id || null,
                })),
              })
              .then(onSaved)
              .catch((err) => setError(err instanceof Error ? err.message : String(err)))
              .finally(() => setSaving(false))
          }}
        >
          <Stack gap="3">
            <Grid min={240} gap="3">
              <TextField name="name" label="Name" defaultValue={person.name} size="sm" required />
              <TextField name="position" label="Position" optional defaultValue={person.position ?? ''} size="sm" />
              <Select
                label="Language"
                size="sm"
                value={language || 'none'}
                onChange={(v) => setLanguage(v === 'none' ? '' : v)}
                options={[
                  { value: 'none', label: 'Not known' },
                  ...Object.entries(LANGUAGES).map(([value, label]) => ({ value, label })),
                ]}
              />
            </Grid>
            <TextArea
              name="brief"
              label="What they are involved in"
              optional
              defaultValue={person.brief ?? ''}
              minRows={2}
            />
            <TagInput
              label="Projects"
              hint="The company's projects they work on. Enter or comma adds one."
              value={projects}
              onChange={setProjects}
              max={20}
            />
            <TextArea
              name="observations"
              label="Observations"
              hint="One per line: “prefers short and quick communication”, “non-technical”."
              defaultValue={person.observations.join('\n')}
              minRows={3}
            />
            <TagInput
              label="Tags"
              hint="Anything that fits no field. Kept in lowercase-kebab."
              value={tags}
              onChange={setTags}
              normalize={kebab}
              splitOnSpace={false}
              max={30}
            />
            <Stack gap="2">
              <Text size="2" weight="medium">
                How to reach them
              </Text>
              {contacts.length === 0 && (
                <Text size="1" tone="muted">
                  No contact yet. The user id is how the fleet recognises their messages.
                </Text>
              )}
              {contacts.map((c, i) => (
                <Row key={`${c.source}-${c.handle}-${i}`} gap="2" justify="between">
                  <Text size="2" breakAll>
                    <Text size="2" tone="muted">
                      {c.source}
                    </Text>{' '}
                    {c.handle}
                    {c.native_id && (
                      <Text size="1" tone="muted" mono>
                        {' '}
                        {c.native_id}
                      </Text>
                    )}
                  </Text>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    aria-label={`Remove ${c.source} ${c.handle}`}
                    onClick={() => setContacts(contacts.filter((_, j) => j !== i))}
                  >
                    Remove
                  </Button>
                </Row>
              ))}
              <Row gap="2" align="end">
                <div style={{ minWidth: '8rem' }}>
                  <Select
                    size="sm"
                    aria-label="Source"
                    value={draft.source}
                    onChange={(v) => setDraft({ ...draft, source: v })}
                    options={sources.map((s) => ({ value: s, label: s }))}
                  />
                </div>
                <div style={{ flexGrow: 1, minWidth: '10rem' }}>
                  <TextField
                    size="sm"
                    aria-label="Handle"
                    placeholder="@name, display name or address"
                    value={draft.handle}
                    onChange={(e) => setDraft({ ...draft, handle: e.currentTarget.value })}
                  />
                </div>
                <div style={{ minWidth: '9rem' }}>
                  <TextField
                    size="sm"
                    aria-label="User id"
                    placeholder="user id (U0123…)"
                    value={draft.native_id}
                    onChange={(e) => setDraft({ ...draft, native_id: e.currentTarget.value })}
                  />
                </div>
                <Button type="button" variant="secondary" size="sm" onClick={addContact}>
                  Add contact
                </Button>
              </Row>
            </Stack>
            {error && <ErrorNote>{error}</ErrorNote>}
            <Row>
              <Button type="submit" loading={saving}>
                Save
              </Button>
            </Row>
          </Stack>
        </form>
      </CardContent>
    </Card>
  )
}
