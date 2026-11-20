import { useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { TextField } from '@/components/ui/text-field'
import { Page, PageHeader, Row, Section, Text } from '@/components/fx/layout'
import { Note } from '@/components/fx/note'
import { Table } from '@/components/fx/table'
import { ArrowLeftIcon } from '@/components/fx/icons'
import * as api from '../api'
import type { SecretFolder as Folder } from '../api'
import { ActionButton, ErrorNote, Loaded, useApi } from '../ui'

export default function SecretFolder() {
  const { id = '' } = useParams()
  const path = `/api/secrets/${encodeURIComponent(id)}`
  const state = useApi<Folder>(path)
  const navigate = useNavigate()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const set = async (form: FormData, formEl: HTMLFormElement) => {
    setBusy(true)
    setError(null)
    try {
      await api.act(path, 'set', { key: form.get('key'), value: form.get('value') })
      // The value must not stay sitting in the page after it is stored.
      formEl.reset()
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
        {(d) => {
          const missing = d.expected.filter((k) => !d.keys.some((x) => x.key === k))
          return (
            <>
              <PageHeader
                title={d.name}
                back={
                  <Link to="/secrets" className="fx-link" data-muted="">
                    <ArrowLeftIcon /> All folders
                  </Link>
                }
                meta={
                  <Row gap="2">
                    {d.preset && d.preset !== 'custom' && <Badge>{d.preset}</Badge>}
                    {d.projects.map((p) => (
                      <Link key={p} to={`/projects/${p}`} className="fx-link">
                        <Badge tone="info">{p}</Badge>
                      </Link>
                    ))}
                    {d.projects.length === 0 && (
                      <Text size="1" tone="muted">
                        Bound to no project, so no agent can read it.
                      </Text>
                    )}
                  </Row>
                }
              />

              {error && <ErrorNote>{error}</ErrorNote>}

              {missing.length > 0 && (
                <Note tone="info" title="This preset also expects">
                  <Text mono>{missing.join(', ')}</Text>
                </Note>
              )}

              <Section title="Keys">
                <Table
                  rows={d.keys}
                  rowKey={(k) => k.key}
                  empty="Nothing stored yet"
                  columns={[
                    {
                      key: 'key',
                      header: 'Key',
                      cell: (k) => (
                        <Text mono size="2">
                          {k.key}
                        </Text>
                      ),
                    },
                    {
                      key: 'act',
                      header: '',
                      align: 'end',
                      cell: (k) => (
                        <ActionButton
                          path={path}
                          op="unset"
                          body={{ key: k.key }}
                          variant="ghost"
                          confirm={`Remove ${k.key}? Anything using it stops working.`}
                          onDone={() => void state.reload()}
                        >
                          Remove
                        </ActionButton>
                      ),
                    },
                  ]}
                />
              </Section>

              <Section title="Set a key">
                <Card>
                  <CardHeader
                    title="Store a value"
                    description="Values are never read back: setting one again is the only way to change it."
                  />
                  <CardContent>
                    <form
                      onSubmit={(e) => {
                        e.preventDefault()
                        void set(new FormData(e.currentTarget), e.currentTarget)
                      }}
                    >
                      <Row gap="3" align="end">
                        <div style={{ minWidth: '13rem' }}>
                          <TextField
                            name="key"
                            label="Key"
                            placeholder="JIRA_API_TOKEN"
                            required
                            list="expected"
                            size="sm"
                          />
                          <datalist id="expected">
                            {d.expected.map((k) => (
                              <option key={k} value={k} />
                            ))}
                          </datalist>
                        </div>
                        <div style={{ minWidth: '16rem', flexGrow: 1 }}>
                          <TextField
                            name="value"
                            label="Value"
                            type="password"
                            autoComplete="off"
                            required
                            size="sm"
                          />
                        </div>
                        <Button type="submit" loading={busy} variant="secondary">
                          Store
                        </Button>
                      </Row>
                    </form>
                  </CardContent>
                </Card>
              </Section>

              <Section title="Danger">
                <Card>
                  <CardContent>
                    <Row gap="3" justify="between">
                      <Text size="2" tone="muted">
                        Deleting a folder deletes every value in it. Nothing can read them back
                        afterwards.
                      </Text>
                      <ActionButton
                        path={path}
                        op="delete"
                        variant="destructive"
                        confirm={`Delete the folder ${d.name} and everything in it?`}
                        onDone={() => navigate('/secrets')}
                      >
                        Delete this folder
                      </ActionButton>
                    </Row>
                  </CardContent>
                </Card>
              </Section>
            </>
          )
        }}
      </Loaded>
    </Page>
  )
}
