import { Badge, Card, Code, Flex, Heading, Text } from '@radix-ui/themes'
import { Link } from 'react-router-dom'
import type { Retro as RetroT } from '../api'
import { ActionButton, Empty, Loaded, Pre, Section, useApi } from '../ui'

const STATE: Record<string, { label: string; color: 'green' | 'gray' | 'amber' }> = {
  open: { label: 'open', color: 'amber' },
  tasked: { label: 'taken up', color: 'green' },
  dismissed: { label: 'dismissed', color: 'gray' },
}

/// What the fleet concluded about its own days, and what it suggests doing about it. The page
/// exists so the proposals are readable (and answerable) outside Telegram.
export default function Retro() {
  const state = useApi<RetroT>('/api/retro')
  const reload = () => void state.reload()

  return (
    <>
      <Heading size="6" mt="5" mb="1">
        Retrospective
      </Heading>
      <Text as="p" size="2" color="gray" mb="3" className="fleet-wrap">
        Once a day the fleet reads its own day — what it did, what you decided, what went wrong —
        and proposes how to be better. A proposal is words until you take it up; then it becomes a
        task on the fleet’s own code, on a branch, nothing pushed.
      </Text>

      <Loaded state={state}>
        {(d) => (
          <>
            {d.proposals.length === 0 ? (
              <Empty>
                No proposals yet. The retrospective runs daily once its hour is set
                (<Code variant="ghost">FLEET_RETRO_HOUR</Code>).
              </Empty>
            ) : (
              <Flex direction="column" gap="3">
                {d.proposals.map((p) => (
                  <Card key={p.id}>
                    <Flex gap="2" align="center" wrap="wrap" mb="1">
                      <Code variant="ghost">P-{p.id}</Code>
                      <Badge color={STATE[p.state]?.color ?? 'gray'} variant="soft">
                        {STATE[p.state]?.label ?? p.state}
                      </Badge>
                      <Badge variant="soft">{p.area}</Badge>
                      {p.task && (
                        <Link className="fleet-row-link" to={`/tasks/${p.task}`}>
                          {p.task}
                        </Link>
                      )}
                    </Flex>
                    <Text as="div" weight="medium" className="fleet-wrap">
                      {p.title}
                    </Text>
                    {/* Written by a session that read the day: shown as text, never markup. */}
                    <Pre>{p.detail}</Pre>
                    {p.state === 'open' && (
                      <Flex gap="2" mt="3">
                        <ActionButton
                          op="make_task"
                          path="/api/retro"
                          body={{ id: p.id }}
                          onDone={reload}
                        >
                          Make task
                        </ActionButton>
                        <ActionButton
                          op="dismiss"
                          path="/api/retro"
                          body={{ id: p.id }}
                          color="gray"
                          variant="soft"
                          onDone={reload}
                        >
                          Dismiss
                        </ActionButton>
                      </Flex>
                    )}
                  </Card>
                ))}
              </Flex>
            )}

            <Section title="Days reviewed">
              {d.runs.length === 0 ? (
                <Empty>None yet.</Empty>
              ) : (
                <Flex direction="column" gap="2">
                  {d.runs.map((r) => (
                    <Card key={r.id}>
                      <Flex gap="2" align="center" wrap="wrap">
                        <Text size="2" weight="medium">
                          {r.day}
                        </Text>
                        <Badge variant="soft">
                          {r.proposals === 1 ? '1 proposal' : `${r.proposals} proposals`}
                        </Badge>
                      </Flex>
                      <Text as="div" size="2" color="gray" mt="1" className="fleet-wrap">
                        {r.verdict ?? 'still thinking…'}
                      </Text>
                    </Card>
                  ))}
                </Flex>
              )}
            </Section>
          </>
        )}
      </Loaded>
    </>
  )
}
