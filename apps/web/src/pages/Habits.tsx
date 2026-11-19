import { Badge, Card, Code, Flex, Heading, Text } from '@radix-ui/themes'
import type { Habits as HabitsT } from '../api'
import { ActionButton, Empty, Loaded, Section, When, useApi } from '../ui'

/// What the fleet does without being asked, because the owner kept doing it themselves. The page
/// exists so there is one place that answers "what acts on its own, and how do I stop it".
export default function Habits() {
  const state = useApi<HabitsT>('/api/habits')
  const reload = () => void state.reload()

  return (
    <>
      <Heading size="6" mt="5" mb="1">
        Habits
      </Heading>
      <Text as="p" size="2" color="gray" mb="3" className="fleet-wrap">
        Things you kept deciding yourself, that the fleet now does for you. Each one was proposed
        and you approved it; pausing one brings the question back to you.
      </Text>
      <Loaded state={state}>
        {(d) =>
          d.habits.length === 0 ? (
            <Empty>
              Nothing yet. When the same kind of decision of yours repeats, you get a card
              offering to take it over.
            </Empty>
          ) : (
            <Flex direction="column" gap="3">
              {d.habits.map((h) => (
                <Card key={h.id}>
                  <Flex gap="2" align="center" wrap="wrap" mb="1">
                    <Code variant="ghost">H-{h.id}</Code>
                    <Badge color={h.active ? 'green' : 'gray'} variant="soft">
                      {h.active ? 'on' : 'paused'}
                    </Badge>
                    <Badge variant="soft">{h.kind === 'decide' ? 'decides' : 'does'}</Badge>
                    {h.last_used_at && (
                      <Text size="1" color="gray">
                        last acted <When at={h.last_used_at} />
                      </Text>
                    )}
                  </Flex>
                  <Text as="div" weight="medium" className="fleet-wrap">
                    {h.what}
                  </Text>
                  <Text as="div" size="2" color="gray" mt="1" className="fleet-wrap">
                    Learned from: {h.why}
                  </Text>
                  {h.paused_why && (
                    <Text as="div" size="2" color="gray" mt="1" className="fleet-wrap">
                      Paused: {h.paused_why}
                    </Text>
                  )}
                  {h.uses.length > 0 && (
                    <Flex direction="column" gap="1" mt="2">
                      <Text size="1" color="gray">
                        Recently
                      </Text>
                      {h.uses.map((u, i) => (
                        <Text key={i} size="1" color="gray" className="fleet-wrap">
                          {u.at} — {u.what}
                        </Text>
                      ))}
                    </Flex>
                  )}
                  <Flex gap="2" mt="3">
                    <ActionButton
                      op={h.active ? 'pause' : 'resume'}
                      path="/api/habits"
                      body={{ id: h.id }}
                      variant="soft"
                      onDone={reload}
                    >
                      {h.active ? 'Pause' : 'Resume'}
                    </ActionButton>
                    <ActionButton
                      op="delete"
                      path="/api/habits"
                      body={{ id: h.id }}
                      color="red"
                      variant="soft"
                      confirm={`Delete H-${h.id}? You will be asked about these yourself again.`}
                      onDone={reload}
                    >
                      Delete
                    </ActionButton>
                  </Flex>
                </Card>
              ))}
            </Flex>
          )
        }
      </Loaded>

      <Section title="How a habit is born">
        <Text as="p" size="2" color="gray" className="fleet-wrap">
          Once a day the orchestrator looks at what you have been deciding and asking for. If the
          same decision has gone your way three times with nothing sent back for changes, it
          offers a habit as an ordinary Approve / Reject card. A habit never answers a plan that
          names a blocker, an open question or an unclear acceptance criterion, never acts more
          than ten times a day, and pauses itself the moment you stop a task it approved.
        </Text>
      </Section>
    </>
  )
}
