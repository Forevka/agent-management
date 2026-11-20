import { useState } from 'react'
import { Link } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { TextField } from '@/components/ui/text-field'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Table } from '@/components/fx/table'
import * as api from '../api'
import type { Overview, ProjectRow } from '../api'
import { ErrorNote, Loaded, useApi } from '../ui'

export default function Projects() {
  const state = useApi<{ projects: ProjectRow[] }>('/api/projects')
  // The per-project counts the overview already works out.
  const overview = useApi<Overview>('/api/overview')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const create = async (form: FormData) => {
    setBusy(true)
    setError(null)
    try {
      await api.act('/api/projects', 'create', {
        slug: form.get('slug'),
        name: form.get('name'),
        description: form.get('description'),
      })
      await Promise.all([state.reload(), overview.reload()])
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Page>
      <PageHeader title="Projects" description="What the fleet works on, and where its work stands." />
      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) => (
          <Table
            rows={d.projects}
            rowKey={(p) => p.slug}
            loading={state.loading}
            empty="No projects yet"
            columns={[
              {
                key: 'project',
                header: 'Project',
                width: '40%',
                cell: (p) => (
                  <Stack gap="1">
                    <Link to={`/projects/${p.slug}`} className="fx-link">
                      {p.name || p.slug}
                    </Link>
                    <Text size="1" tone="muted" mono>
                      {p.slug}
                    </Text>
                  </Stack>
                ),
              },
              {
                key: 'jira',
                header: 'Jira',
                secondary: true,
                cell: (p) =>
                  p.jira ? (
                    <Badge>{p.jira}</Badge>
                  ) : (
                    <Text size="1" tone="muted">
                      —
                    </Text>
                  ),
              },
              {
                key: 'tasks',
                header: 'Tasks',
                align: 'end',
                cell: (p) => {
                  const counts = overview.data?.projects.find((x) => x.slug === p.slug)
                  if (!counts)
                    return (
                      <Text size="1" tone="muted">
                        —
                      </Text>
                    )
                  return (
                    <Row gap="2" justify="end">
                      {counts.running > 0 && (
                        <Badge tone="success" live>
                          {counts.running} live
                        </Badge>
                      )}
                      <Text size="1">
                        {counts.open} open of {counts.total}
                      </Text>
                    </Row>
                  )
                },
              },
            ]}
          />
        )}
      </Loaded>

      <Section title="New project">
        <Card>
          <CardHeader
            title="Add a project"
            description="The slug prefixes every task id, so keep it short — it is permanent."
          />
          <CardContent>
            <form
              onSubmit={(e) => {
                e.preventDefault()
                void create(new FormData(e.currentTarget))
              }}
            >
              <Row gap="3" align="end">
                <div style={{ minWidth: '9rem' }}>
                  <TextField name="slug" label="Slug" placeholder="bioins" required size="sm" />
                </div>
                <div style={{ minWidth: '12rem', flexGrow: 1 }}>
                  <TextField name="name" label="Name" placeholder="Bio Insights" optional size="sm" />
                </div>
                <div style={{ minWidth: '14rem', flexGrow: 2 }}>
                  <TextField name="description" label="Description" optional size="sm" />
                </div>
                <Button type="submit" loading={busy} variant="secondary">
                  Create
                </Button>
              </Row>
            </form>
          </CardContent>
        </Card>
      </Section>
    </Page>
  )
}
