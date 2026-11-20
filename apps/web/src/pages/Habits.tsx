import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardFooter, CardHeader } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import type { Habits as HabitsT } from '../api'
import { ActionButton, Loaded, When, useApi } from '../ui'

/// What the fleet does without being asked, because the owner kept doing it themselves. The page
/// exists so there is one place that answers "what acts on its own, and how do I stop it".
export default function Habits() {
  const state = useApi<HabitsT>('/api/habits')
  const reload = () => void state.reload()

  return (
    <Page>
      <PageHeader
        title="Habits"
        description="Things you kept deciding yourself, that the fleet now does for you. Each one was proposed and you approved it; pausing one brings the question back to you."
      />
      <Loaded state={state}>
        {(d) =>
          d.habits.length === 0 ? (
            <EmptyState
              title="No habits yet"
              text="When the same kind of decision of yours repeats, you get a card offering to take it over."
            />
          ) : (
            <Stack gap="3">
              {d.habits.map((h) => (
                <Card key={h.id}>
                  <CardHeader
                    title={h.what}
                    description={`Learned from: ${h.why}`}
                    action={
                      <Row gap="2" justify="end">
                        <Badge tone={h.active ? 'success' : 'neutral'} dot>
                          {h.active ? 'on' : 'paused'}
                        </Badge>
                        <Badge>{h.kind === 'decide' ? 'decides' : 'does'}</Badge>
                      </Row>
                    }
                  />
                  <CardContent>
                    <Stack gap="2">
                      <Row gap="3">
                        <Text size="1" tone="muted" mono>
                          H-{h.id}
                        </Text>
                        <Text size="1" tone="muted">
                          up to {h.cap}/day
                        </Text>
                        {h.last_used_at && (
                          <Row gap="1">
                            <Text size="1" tone="muted">
                              last acted
                            </Text>
                            <When at={h.last_used_at} />
                          </Row>
                        )}
                      </Row>
                      {h.paused_why && (
                        <Text size="1" tone="muted" breakAll>
                          Paused: {h.paused_why}
                        </Text>
                      )}
                      {h.uses.length > 0 && (
                        <Stack gap="1">
                          <Text size="1" tone="muted" weight="medium">
                            Recently
                          </Text>
                          {h.uses.map((u, i) => (
                            <Text key={i} size="1" tone="muted" breakAll>
                              {u.at} — {u.what}
                            </Text>
                          ))}
                        </Stack>
                      )}
                    </Stack>
                  </CardContent>
                  <CardFooter>
                    <Row gap="2">
                      <ActionButton
                        op={h.active ? 'pause' : 'resume'}
                        path="/api/habits"
                        body={{ id: h.id }}
                        onDone={reload}
                      >
                        {h.active ? 'Pause' : 'Resume'}
                      </ActionButton>
                      <ActionButton
                        op="delete"
                        path="/api/habits"
                        body={{ id: h.id }}
                        variant="destructive"
                        confirm={`Delete H-${h.id}? You will be asked about these yourself again.`}
                        onDone={reload}
                      >
                        Delete
                      </ActionButton>
                    </Row>
                  </CardFooter>
                </Card>
              ))}
            </Stack>
          )
        }
      </Loaded>

      <Section title="How a habit is born">
        <Card>
          <CardContent>
            <Text as="p" size="2" tone="muted">
              Once a day the orchestrator looks at what you have been deciding and asking for. If
              the same decision has gone your way three times in a fortnight with nothing sent back
              for changes, it offers a habit as an ordinary Approve / Reject card — and only your
              tap creates one. A habit never answers a plan that names a blocker, an open question
              or an unclear acceptance criterion, never answers while another question of that task
              is open, and stands aside for good once you send any plan of that shape back. Each one
              is capped per day (see each habit above), and the card it leaves in the task’s topic
              has a button that stops the task and pauses the habit.
            </Text>
          </CardContent>
        </Card>
      </Section>
    </Page>
  )
}
