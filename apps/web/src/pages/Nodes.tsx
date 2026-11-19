import { useState } from 'react'
import { Badge, Box, Button, Callout, Card, Code, Flex, Heading, Table, Text } from '@radix-ui/themes'
import { Link } from 'react-router-dom'
import * as api from '../api'
import type { Node } from '../api'
import { ErrorNote, Loaded, Section, When, useApi } from '../ui'

export default function Nodes() {
  const state = useApi<{ nodes: Node[] }>('/api/nodes', 15_000)
  const [ticket, setTicket] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const invite = async () => {
    setBusy(true)
    setError(null)
    try {
      const r = await api.act<{ ticket: string }>('/api/invite', 'invite')
      setTicket(r.ticket)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <Heading size="6" mt="5" mb="3">
        Nodes
      </Heading>
      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) => (
          <Table.Root variant="surface">
            <Table.Header>
              <Table.Row>
                <Table.ColumnHeaderCell>Node</Table.ColumnHeaderCell>
                <Table.ColumnHeaderCell>Status</Table.ColumnHeaderCell>
                <Table.ColumnHeaderCell>Projects</Table.ColumnHeaderCell>
                <Table.ColumnHeaderCell>Seen</Table.ColumnHeaderCell>
              </Table.Row>
            </Table.Header>
            <Table.Body>
              {d.nodes.map((n) => (
                <Table.Row key={n.id}>
                  <Table.Cell>
                    <Link className="fleet-row-link" to={`/nodes/${n.id}`}>
                      {n.name}
                    </Link>
                    <Text as="div" size="1" color="gray">
                      {n.os} · {n.version}
                    </Text>
                  </Table.Cell>
                  <Table.Cell>
                    <Flex gap="2" align="center">
                      <Badge color={n.online ? 'green' : 'gray'} variant="soft">
                        {n.online ? 'online' : 'offline'}
                      </Badge>
                      <Text size="2" color="gray">
                        {n.running.length}/{n.max_agents}
                      </Text>
                    </Flex>
                  </Table.Cell>
                  <Table.Cell>
                    <Flex gap="1" wrap="wrap">
                      {n.projects.map((p) => (
                        <Badge key={p.project} variant="soft">
                          {p.project}
                        </Badge>
                      ))}
                    </Flex>
                  </Table.Cell>
                  <Table.Cell>
                    <When at={n.last_seen} />
                  </Table.Cell>
                </Table.Row>
              ))}
            </Table.Body>
          </Table.Root>
        )}
      </Loaded>

      <Section title="Add a node">
        <Card>
          <Text as="p" size="2" color="gray" mb="3">
            An invite is single-use. Run <Code>fleet join &lt;ticket&gt;</Code> on the machine.
          </Text>
          <Button onClick={() => void invite()} loading={busy}>
            Create an invite
          </Button>
          {ticket && (
            <Callout.Root mt="3">
              <Callout.Text>
                <Text as="div" size="1" color="gray" mb="1">
                  Copy it now — it is not shown again.
                </Text>
                <Box
                  style={{
                    userSelect: 'all',
                    overflowWrap: 'anywhere',
                    fontFamily: 'var(--code-font-family)',
                    fontSize: 'var(--font-size-1)',
                  }}
                >
                  {ticket}
                </Box>
              </Callout.Text>
            </Callout.Root>
          )}
        </Card>
      </Section>
    </>
  )
}
