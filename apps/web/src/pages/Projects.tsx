import { useState } from 'react'
import { Badge, Box, Button, Card, Code, Flex, Heading, Table, Text, TextField } from '@radix-ui/themes'
import { Link } from 'react-router-dom'
import * as api from '../api'
import type { Overview, Project } from '../api'
import { ErrorNote, Loaded, Section, useApi } from '../ui'

export default function Projects() {
  const state = useApi<{ projects: Project[] }>('/api/projects')
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
    <>
      <Heading size="6" mt="5" mb="3">
        Projects
      </Heading>
      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) => (
          <Table.Root variant="surface">
            <Table.Header>
              <Table.Row>
                <Table.ColumnHeaderCell>Project</Table.ColumnHeaderCell>
                <Table.ColumnHeaderCell>Jira</Table.ColumnHeaderCell>
                <Table.ColumnHeaderCell>Tasks</Table.ColumnHeaderCell>
              </Table.Row>
            </Table.Header>
            <Table.Body>
              {d.projects.map((p) => {
                const counts = overview.data?.projects.find((x) => x.slug === p.slug)
                return (
                  <Table.Row key={p.slug}>
                    <Table.Cell>
                      <Link className="fleet-row-link" to={`/projects/${p.slug}`}>
                        {p.name || p.slug}
                      </Link>
                      <Text as="div" size="1" color="gray">
                        <Code variant="ghost">{p.slug}</Code>
                      </Text>
                    </Table.Cell>
                    <Table.Cell>
                      {p.jira?.key ? (
                        <Badge variant="soft">{p.jira.key}</Badge>
                      ) : (
                        <Text size="2" color="gray">
                          —
                        </Text>
                      )}
                    </Table.Cell>
                    <Table.Cell>
                      {counts ? (
                        <Flex gap="2" align="center">
                          {counts.running > 0 && (
                            <Badge color="green" variant="soft">
                              {counts.running} live
                            </Badge>
                          )}
                          <Text size="2">
                            {counts.open} open of {counts.total}
                          </Text>
                        </Flex>
                      ) : (
                        <Text size="2" color="gray">
                          —
                        </Text>
                      )}
                    </Table.Cell>
                  </Table.Row>
                )
              })}
            </Table.Body>
          </Table.Root>
        )}
      </Loaded>

      <Section title="New project">
        <Card>
          <form
            onSubmit={(e) => {
              e.preventDefault()
              void create(new FormData(e.currentTarget))
            }}
          >
            <Flex gap="2" align="end" wrap="wrap">
              <label style={{ minWidth: '9rem' }}>
                <Text as="div" size="2" mb="1" weight="medium">
                  Slug
                </Text>
                {/* The slug prefixes every task id, so it is short and permanent. */}
                <TextField.Root name="slug" placeholder="bioins" required />
              </label>
              <label style={{ minWidth: '12rem', flexGrow: 1 }}>
                <Text as="div" size="2" mb="1" weight="medium">
                  Name
                </Text>
                <TextField.Root name="name" placeholder="Bio Insights" />
              </label>
              <label style={{ minWidth: '14rem', flexGrow: 2 }}>
                <Text as="div" size="2" mb="1" weight="medium">
                  Description
                </Text>
                <TextField.Root name="description" />
              </label>
              <Box>
                <Button type="submit" loading={busy}>
                  Create
                </Button>
              </Box>
            </Flex>
          </form>
        </Card>
      </Section>
    </>
  )
}
