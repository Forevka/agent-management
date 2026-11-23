import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardFooter, CardHeader } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import type { Habits as HabitsT } from '../api'
import { ActionButton, Loaded, When, useApi } from '../ui'

type Entry = HabitsT['habits'][number]['log'][number]

/// How each line of a habit's log reads at a glance: acting is good news, standing aside is
/// worth a look, failing is not.
const TONE: Record<Entry['kind'], 'success' | 'neutral' | 'warning' | 'danger' | 'info'> = {
  fired: 'success',
  skipped: 'warning',
  failed: 'danger',
  report: 'info',
}

/// The habit's own matching "stands aside"; the orchestrator, handling a standing request,
/// "passes over" a candidate (and says why).
function label(kind: Entry['kind'], source: Entry['source']): string {
  switch (kind) {
    case 'fired':
      return 'acted'
    case 'skipped':
      return source === 'brain' ? 'passed over' : 'stood aside'
    case 'failed':
      return 'failed'
    case 'report':
      return 'report'
  }
}

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
                        {h.project && <Badge tone="info">{h.project}</Badge>}
                        <Text size="1" tone="muted">
                          up to {h.cap}/day
                        </Text>
                        {h.last_checked_at && (
                          <Row gap="1">
                            <Text size="1" tone="muted">
                              last checked
                            </Text>
                            <When at={h.last_checked_at} />
                          </Row>
                        )}
                        {h.last_fired_at && (
                          <Row gap="1">
                            <Text size="1" tone="muted">
                              last acted
                            </Text>
                            <When at={h.last_fired_at} />
                          </Row>
                        )}
                      </Row>
                      <Text size="2" breakAll>
                        {h.status}
                      </Text>
                      {h.rules.map((r) => (
                        <Row key={r.id} gap="2" align="start">
                          <Badge tone={r.active ? 'info' : 'warning'}>OR-{r.id}</Badge>
                          <Text size="1" tone="muted" breakAll>
                            {r.active ? r.text : `${r.text} (no longer holds)`}
                          </Text>
                        </Row>
                      ))}
                      {h.log.length > 0 && (
                        <Stack gap="1">
                          <Text size="1" tone="muted" weight="medium">
                            Log
                          </Text>
                          {h.log.map((e, i) => (
                            <Row key={i} gap="2" align="start">
                              <When at={e.at} />
                              <Badge tone={TONE[e.kind]}>{label(e.kind, e.source)}</Badge>
                              <Text size="1" tone="muted" breakAll>
                                {e.candidate && e.kind !== 'fired' ? `${e.candidate}: ` : ''}
                                {e.what}
                              </Text>
                            </Row>
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
            <Stack gap="2">
              <Text as="p" size="2" tone="muted">
                Once a day the orchestrator looks at what you have been deciding and asking for. If
                the same decision has gone your way three times in a fortnight with nothing sent
                back for changes, it offers a habit as an ordinary Approve / Reject card — and only
                your tap creates one. A habit never answers a plan that names a blocker, an open
                question or an unclear acceptance criterion, never answers while another question of
                that task is open, and stands aside for good once you send any plan of that shape
                back. Each one is capped per day (see each habit above), and the card it leaves in
                the task’s topic has a button that stops the task and pauses the habit.
              </Text>
              <Text as="p" size="2" tone="muted">
                A standing duty you state yourself (“every day review two fresh MRs”, “always keep
                the Jira status current”) is offered as a habit straight away, with your words, a
                schedule and its project, instead of being kept in the orchestrator’s memory. Each
                habit keeps a log: when it was last checked, when it last acted, what it stood aside
                from and why, and the orchestrator’s report of what it did with each request and
                what it passed over. So “did something go wrong?” is answered here.
              </Text>
              <Text as="p" size="2" tone="muted">
                A habit that acts in your name — approving or merging, posting or commenting,
                moving a ticket, replying as you — cites the rule of yours it follows (OR-n), and
                each request carries that rule as it stands now. When you replace or remove a rule,
                the habits that cite it are paused until you have checked their briefs.
              </Text>
            </Stack>
          </CardContent>
        </Card>
      </Section>
    </Page>
  )
}
