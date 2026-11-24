import { useState } from 'react'
import { Link } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { TextField } from '@/components/ui/text-field'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Note } from '@/components/fx/note'
import { Table } from '@/components/fx/table'
import * as api from '../api'
import type { Node } from '../api'
import { DiskBadge } from '../disk'
import { UsageBadge } from '../usage'
import { ErrorNote, Loaded, When, useApi } from '../ui'

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
    <Page>
      <PageHeader title="Nodes" description="The machines that run agents, and what each is doing." />
      {error && <ErrorNote>{error}</ErrorNote>}

      <Loaded state={state}>
        {(d) => (
          <Table
            rows={d.nodes}
            rowKey={(n) => n.id}
            loading={state.loading}
            empty="No node has joined yet"
            columns={[
              {
                key: 'node',
                header: 'Node',
                width: '32%',
                cell: (n) => (
                  <Stack gap="1">
                    <Link to={`/nodes/${n.id}`} className="fx-link">
                      {n.name}
                    </Link>
                    <Text size="1" tone="muted">
                      {n.os} · {n.version}
                    </Text>
                  </Stack>
                ),
              },
              {
                key: 'status',
                header: 'Status',
                cell: (n) => (
                  <Row gap="2">
                    <Badge tone={n.online ? 'success' : 'neutral'} dot live={n.online || undefined}>
                      {n.online ? 'online' : 'offline'}
                    </Badge>
                    <Text size="1" tone="muted">
                      {n.running.length}/{n.max_agents} agents
                    </Text>
                  </Row>
                ),
              },
              {
                key: 'disk',
                header: 'Disk',
                cell: (n) => <DiskBadge disk={n.disk} />,
              },
              {
                key: 'usage',
                header: 'Claude usage',
                cell: (n) => <UsageBadge usage={n.claude_usage} />,
              },
              {
                key: 'projects',
                header: 'Projects',
                secondary: true,
                cell: (n) =>
                  n.projects.length === 0 ? (
                    <Text size="1" tone="muted">
                      none mapped
                    </Text>
                  ) : (
                    <Row gap="1">
                      {n.projects.map((p) => (
                        <Badge key={p.project}>{p.project}</Badge>
                      ))}
                    </Row>
                  ),
              },
              { key: 'seen', header: 'Seen', align: 'end', cell: (n) => <When at={n.last_seen} /> },
            ]}
          />
        )}
      </Loaded>

      <Section title="Add a node">
        <Card>
          <CardHeader
            title="One machine, one invite"
            description="An invite is single-use and expires. Run it on the machine that should join."
          />
          <CardContent>
            <Stack gap="3">
              <Text size="2" tone="muted">
                Run <Text mono>fleet join &lt;ticket&gt;</Text> there.
              </Text>
              <Row>
                <Button onClick={() => void invite()} loading={busy} variant="secondary">
                  Create an invite
                </Button>
              </Row>
              {ticket && (
                <Note tone="warning" title="Copy it now — it is not shown again">
                  <TextField
                    value={ticket}
                    readOnly
                    copyable
                    aria-label="Invite ticket"
                    onFocus={(e) => e.currentTarget.select()}
                  />
                </Note>
              )}
            </Stack>
          </CardContent>
        </Card>
      </Section>
    </Page>
  )
}
