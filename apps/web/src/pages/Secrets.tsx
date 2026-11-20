import { useState } from 'react'
import { Link } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { Select } from '@/components/ui/select'
import { TextField } from '@/components/ui/text-field'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Note } from '@/components/fx/note'
import { Table } from '@/components/fx/table'
import * as api from '../api'
import type { Secrets as SecretsT } from '../api'
import { ErrorNote, Loaded, useApi } from '../ui'

export default function Secrets() {
  const state = useApi<SecretsT>('/api/secrets')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [preset, setPreset] = useState('custom')

  const create = async (form: FormData) => {
    setBusy(true)
    setError(null)
    try {
      await api.act('/api/secrets', 'create', { name: form.get('name'), preset })
      await state.reload()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Page>
      <PageHeader title="Secrets" description="Folders of key/value pairs an agent can be given for one run." />

      <Note tone="info" title="Write-only on purpose">
        Values are encrypted at rest and never shown again — not here, and not to an agent that
        reads them. A folder reaches an agent only as a <Text mono>.env</Text> file for one run,
        outside the project directory, deleted when the agent exits.
      </Note>

      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) => (
          <>
            <Table
              rows={d.folders}
              rowKey={(f) => String(f.id)}
              loading={state.loading}
              empty="No folders yet"
              columns={[
                {
                  key: 'folder',
                  header: 'Folder',
                  width: '40%',
                  cell: (f) => (
                    <Stack gap="1">
                      <Link to={`/secrets/${f.id}`} className="fx-link">
                        {f.name}
                      </Link>
                      {f.preset && f.preset !== 'custom' && (
                        <Text size="1" tone="muted">
                          {f.preset}
                        </Text>
                      )}
                    </Stack>
                  ),
                },
                { key: 'keys', header: 'Keys', numeric: true, cell: (f) => <Text size="1">{f.keys}</Text> },
                {
                  key: 'projects',
                  header: 'Projects',
                  cell: (f) =>
                    f.projects.length === 0 ? (
                      <Text size="1" tone="muted">
                        bound to none
                      </Text>
                    ) : (
                      <Row gap="1">
                        {f.projects.map((p) => (
                          <Badge key={p}>{p}</Badge>
                        ))}
                      </Row>
                    ),
                },
              ]}
            />

            <Section title="New folder">
              <Card>
                <CardHeader
                  title="Add a folder"
                  description="A preset only suggests the keys; a folder may hold anything."
                />
                <CardContent>
                  <form
                    onSubmit={(e) => {
                      e.preventDefault()
                      void create(new FormData(e.currentTarget))
                    }}
                  >
                    <Row gap="3" align="end">
                      <div style={{ minWidth: '14rem', flexGrow: 1 }}>
                        <TextField name="name" label="Name" placeholder="jira" required size="sm" />
                      </div>
                      <div style={{ minWidth: '12rem' }}>
                        <Select
                          label="Preset"
                          size="sm"
                          value={preset}
                          onChange={setPreset}
                          options={d.presets.map((p) => ({
                            value: p.name,
                            label: p.name,
                            description: p.keys.length > 0 ? `${p.keys.length} keys` : undefined,
                          }))}
                        />
                      </div>
                      <Button type="submit" loading={busy} variant="secondary">
                        Create
                      </Button>
                    </Row>
                  </form>
                </CardContent>
              </Card>
            </Section>
          </>
        )}
      </Loaded>
    </Page>
  )
}
