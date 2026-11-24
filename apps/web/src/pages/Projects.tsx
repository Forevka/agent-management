import { useState } from 'react'
import { Link } from 'react-router-dom'
import { Folder01Icon } from '@hugeicons/core-free-icons'
import { Icon } from '@/components/ui/app-shell'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { TextField } from '@/components/ui/text-field'
import { Grid, Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
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
      <style href="fx-projects" precedence="default">
        {css}
      </style>
      <PageHeader title="Projects" description="What the fleet works on, and where its work stands." />
      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) =>
          d.projects.length === 0 ? (
            <EmptyState title="No projects yet" text="Add one below; it becomes a folder in the sidebar." />
          ) : (
            // Each project is a folder, as in the sidebar: a card that opens its directory.
            <Grid min={260} gap="3">
              {d.projects.map((p) => {
                const counts = overview.data?.projects.find((x) => x.slug === p.slug)
                return (
                  <Card key={p.slug} interactive>
                    <Link to={`/projects/${encodeURIComponent(p.slug)}`} className="fx-item">
                      <Row gap="2" wrap={false}>
                        <span className="fx-projects__folder" data-project={p.color} aria-hidden="true">
                          <Icon icon={Folder01Icon} size={18} />
                        </span>
                        <Stack gap="0">
                          <Text weight="medium">{p.name || p.slug}</Text>
                          <Text size="1" tone="muted" mono>
                            {p.slug}
                          </Text>
                        </Stack>
                      </Row>
                      {p.description && (
                        <Text size="1" tone="muted" className="fx-projects__desc">
                          {p.description}
                        </Text>
                      )}
                      <Row gap="2">
                        {counts && counts.running > 0 && (
                          <Badge tone="success" live>
                            {counts.running} live
                          </Badge>
                        )}
                        {/* Only the overview's counts: the list's own `open_tasks` counts stopped
                            and failed work too, and a number that changes as the page loads is
                            worse than none. */}
                        {counts && (
                          <Text size="1">
                            {counts.open} open of {counts.total}
                          </Text>
                        )}
                        {p.jira && <Badge>{p.jira}</Badge>}
                      </Row>
                    </Link>
                  </Card>
                )
              })}
            </Grid>
          )
        }
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

const css = /* css */ `
/* The folder in its project's hue: a soft tile with the hue on the icon. */
.fx-projects__folder[data-project] {
  background: var(--fx-project-soft);
  color: var(--fx-project);
}
.fx-projects__folder {
  display: grid;
  place-items: center;
  flex: none;
  width: 34px;
  height: 34px;
  border-radius: var(--aui-radius-sm, 7px);
  background: var(--aui-subtle, #f0f0f0);
  color: var(--aui-muted, #737373);
}
.fx-projects__desc {
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}
`
