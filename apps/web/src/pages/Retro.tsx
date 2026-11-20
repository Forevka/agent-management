import { Link } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardFooter, CardHeader } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import type { Retro as RetroT } from '../api'
import { ActionButton, Loaded, Pre, useApi } from '../ui'

const STATE: Record<string, { label: string; tone: 'success' | 'neutral' | 'warning' }> = {
  open: { label: 'open', tone: 'warning' },
  tasked: { label: 'taken up', tone: 'success' },
  dismissed: { label: 'dismissed', tone: 'neutral' },
}

/// What the fleet concluded about its own days, and what it suggests doing about it. The page
/// exists so the proposals are readable (and answerable) outside Telegram.
export default function Retro() {
  const state = useApi<RetroT>('/api/retro')
  const reload = () => void state.reload()

  return (
    <Page>
      <PageHeader
        title="Retrospective"
        description="Once a day the fleet reads its own day — what it did, what you decided, what went wrong — and proposes how to be better. A proposal is words until you take it up; then it becomes a task on the fleet’s own code, on a branch, nothing pushed."
      />

      <Loaded state={state}>
        {(d) => (
          <>
            {d.proposals.length === 0 ? (
              <EmptyState
                title="No proposals yet"
                text="The retrospective runs daily once its hour is set (FLEET_RETRO_HOUR)."
              />
            ) : (
              <Stack gap="3">
                {d.proposals.map((p) => (
                  <Card key={p.id}>
                    <CardHeader
                      title={p.title}
                      action={
                        <Row gap="2" justify="end">
                          <Badge tone={STATE[p.state]?.tone ?? 'neutral'} dot>
                            {STATE[p.state]?.label ?? p.state}
                          </Badge>
                          <Badge>{p.area}</Badge>
                        </Row>
                      }
                    />
                    <CardContent>
                      <Stack gap="2">
                        <Row gap="3">
                          <Text size="1" tone="muted" mono>
                            P-{p.id}
                          </Text>
                          {p.task && (
                            <Link to={`/tasks/${p.task}`} className="fx-link">
                              {p.task}
                            </Link>
                          )}
                        </Row>
                        {/* Written by a session that read the day: shown as text, never markup. */}
                        <Pre>{p.detail}</Pre>
                      </Stack>
                    </CardContent>
                    {p.state === 'open' && (
                      <CardFooter>
                        <Row gap="2">
                          <ActionButton
                            op="make_task"
                            path="/api/retro"
                            body={{ id: p.id }}
                            variant="primary"
                            onDone={reload}
                          >
                            Make task
                          </ActionButton>
                          <ActionButton op="dismiss" path="/api/retro" body={{ id: p.id }} onDone={reload}>
                            Dismiss
                          </ActionButton>
                        </Row>
                      </CardFooter>
                    )}
                  </Card>
                ))}
              </Stack>
            )}

            <Section title="Days reviewed">
              {d.runs.length === 0 ? (
                <EmptyState title="No day reviewed yet" bare />
              ) : (
                <Stack gap="2">
                  {d.runs.map((r) => (
                    <Card key={r.id}>
                      <CardContent>
                        <Stack gap="2">
                          <Row gap="2">
                            <Text size="2" weight="medium">
                              {r.day}
                            </Text>
                            <Badge>{r.proposals === 1 ? '1 proposal' : `${r.proposals} proposals`}</Badge>
                          </Row>
                          <Text size="2" tone="muted" breakAll>
                            {r.verdict ?? 'still thinking…'}
                          </Text>
                        </Stack>
                      </CardContent>
                    </Card>
                  ))}
                </Stack>
              )}
            </Section>
          </>
        )}
      </Loaded>
    </Page>
  )
}
