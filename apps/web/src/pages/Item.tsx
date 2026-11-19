import { useState } from 'react'
import {
  Badge, Box, Button, Callout, Card, Checkbox, Code, Flex, Heading, RadioGroup, Text, TextArea,
} from '@radix-ui/themes'
import { CheckCircledIcon, InfoCircledIcon } from '@radix-ui/react-icons'
import { Link, useParams } from 'react-router-dom'
import * as api from '../api'
import { HOLDS_THE_AGENT } from '../api'
import type { Item as ItemT } from '../api'
import { ErrorNote, Loaded, Pre, useApi } from '../ui'

const str = (v: unknown): string => (typeof v === 'string' ? v : v === undefined || v === null ? '' : JSON.stringify(v, null, 2))

/// Answers to an `AskUserQuestion`, keyed the way the server wants them: the item's id, then the
/// question's own text, then the labels picked. The server maps labels back itself, so nothing
/// here needs to know the real options.
type Answers = Record<number, Record<string, string[]>>

function Questions({ item, answers, set }: { item: ItemT; answers: Answers; set: (a: Answers) => void }) {
  const questions = (item.payload?.questions as Record<string, unknown>[]) ?? []
  return (
    <>
      {questions.map((q, qi) => {
        const text = str(q.question)
        const multi = q.multiSelect === true
        const options = (q.options as Record<string, unknown>[]) ?? []
        const picked = answers[item.id]?.[text] ?? []
        const pick = (label: string, on: boolean) => {
          const next = multi
            ? on
              ? [...picked, label]
              : picked.filter((p) => p !== label)
            : [label]
          set({ ...answers, [item.id]: { ...(answers[item.id] ?? {}), [text]: next } })
        }
        return (
          <Card key={qi} mb="2">
            <Text as="div" weight="medium" mb="2" className="fleet-wrap">
              {text}
            </Text>
            {multi ? (
              <Flex direction="column" gap="2">
                {options.map((o, oi) => (
                  <Text as="label" size="2" key={oi}>
                    <Flex gap="2">
                      <Checkbox
                        checked={picked.includes(str(o.label))}
                        onCheckedChange={(v) => pick(str(o.label), v === true)}
                      />
                      <Box>
                        <Text as="div">{str(o.label)}</Text>
                        <Text as="div" size="1" color="gray" className="fleet-wrap">
                          {str(o.description)}
                        </Text>
                      </Box>
                    </Flex>
                  </Text>
                ))}
              </Flex>
            ) : (
              <RadioGroup.Root value={picked[0] ?? ''} onValueChange={(v) => pick(v, true)}>
                <Flex direction="column" gap="2">
                  {options.map((o, oi) => (
                    <Text as="label" size="2" key={oi}>
                      <Flex gap="2">
                        <RadioGroup.Item value={str(o.label)} />
                        <Box>
                          <Text as="div">{str(o.label)}</Text>
                          <Text as="div" size="1" color="gray" className="fleet-wrap">
                            {str(o.description)}
                          </Text>
                        </Box>
                      </Flex>
                    </Text>
                  ))}
                </Flex>
              </RadioGroup.Root>
            )}
          </Card>
        )
      })}
    </>
  )
}

/// The kinds this page has a form for. Anything else still has to be readable, so it falls back
/// to its payload as text: an item the owner cannot see is an item they cannot answer.
const WITH_FORM = ['plan', 'question', 'permission', 'ask', 'admin_change', 'task_proposal', 'slack_reply', 'reply']

export default function Item() {
  const { id = '' } = useParams()
  const path = `/api/items/${encodeURIComponent(id)}`
  const state = useApi<ItemT>(path)
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [done, setDone] = useState<string[] | null>(null)
  const [feedback, setFeedback] = useState('')
  const [text, setText] = useState('')
  const [choice, setChoice] = useState<string>('')
  const [answers, setAnswers] = useState<Answers>({})

  const send = async (op: string, body: Record<string, unknown> = {}) => {
    setBusy(op)
    setError(null)
    try {
      const questions = Object.keys(answers).length > 0 ? { questions: answers } : {}
      const r = await api.act<{ done: string[] }>(path, op, { ...body, ...questions })
      setDone(r.done)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  if (done) {
    return (
      <Box mt="5">
        <Callout.Root color="green" mb="3">
          <Callout.Icon>
            <CheckCircledIcon />
          </Callout.Icon>
          <Callout.Text>
            {done.join(' · ') || 'Answered.'}
          </Callout.Text>
        </Callout.Root>
        <Link to="/">Back to the dashboard</Link>
      </Box>
    )
  }

  return (
    <Loaded state={state}>
      {(it) => {
        const p = it.payload ?? {}
        const plan = it.kind === 'plan' ? it : it.related?.find((r) => r.kind === 'plan')
        const questions = [it, ...(it.related ?? [])].filter((i) => i.kind === 'question')
        return (
          <>
            <Flex align="center" gap="3" mt="5" mb="1" wrap="wrap">
              <Heading size="6">{plan ? 'Plan review' : `Q-${it.id}`}</Heading>
              <Badge color={it.blocking ? 'red' : 'amber'} variant="soft">
                {it.kind}
              </Badge>
              {it.task && (
                <Link className="fleet-row-link" to={`/tasks/${it.task}`}>
                  {it.task}
                </Link>
              )}
              {it.agent && (
                <Text size="2" color="gray">
                  {it.agent}
                </Text>
              )}
            </Flex>
            {/* What is waiting, in words, so the question can be answered from this page alone. */}
            {(() => {
              const waiting = HOLDS_THE_AGENT.includes(it.kind)
                ? 'the agent is stopped until you answer.'
                : it.kind === 'ask'
                  ? 'your answer reaches the agent as a message; it keeps working meanwhile.'
                  : ''
              return (
                (it.task_title || waiting) && (
                  <Text as="div" size="2" color="gray" mb="2" className="fleet-wrap">
                    {it.task_title}
                    {it.task_title && waiting ? ' — ' : ''}
                    {waiting}
                  </Text>
                )
              )
            })()}
            {it.state && it.state !== 'open' && (
              <Callout.Root color="gray" my="3">
                <Callout.Icon>
                  <InfoCircledIcon />
                </Callout.Icon>
                <Callout.Text>This was already {it.state}.</Callout.Text>
              </Callout.Root>
            )}

            {error && <ErrorNote>{error}</ErrorNote>}

            {/* A plan is shown as text, not rendered markup: an agent writes it. */}
            {plan && (
              <Card my="3">
                <Text as="div" size="2" color="gray" mb="2">
                  The agent’s plan
                </Text>
                <Pre>{str(plan.payload?.plan ?? plan.summary)}</Pre>
              </Card>
            )}

            {questions.length > 0 && (
              <Box my="3">
                <Heading size="3" mb="2">
                  Questions
                </Heading>
                {questions.map((q) => (
                  <Questions key={q.id} item={q} answers={answers} set={setAnswers} />
                ))}
                {!plan && (
                  <Button onClick={() => void send('questions')} loading={busy === 'questions'}>
                    Submit answers
                  </Button>
                )}
              </Box>
            )}

            {it.kind === 'ask' && (
              <Card my="3">
                <Text as="div" weight="medium" mb="2" className="fleet-wrap">
                  {str(p.question) || it.summary}
                </Text>
                <RadioGroup.Root value={choice} onValueChange={setChoice}>
                  <Flex direction="column" gap="2">
                    {((p.options as unknown[]) ?? []).map((o, i) => (
                      <Text as="label" size="2" key={i}>
                        <Flex gap="2" align="center">
                          <RadioGroup.Item value={String(i)} />
                          {str(o)}
                        </Flex>
                      </Text>
                    ))}
                  </Flex>
                </RadioGroup.Root>
                <Box mt="3">
                  <Text as="div" size="2" mb="1">
                    Or answer in your own words
                  </Text>
                  <TextArea value={text} onChange={(e) => setText(e.currentTarget.value)} />
                </Box>
                <Box mt="3">
                  <Button
                    onClick={() =>
                      void send('answer', text.trim() ? { text } : { choice: Number(choice) })
                    }
                    loading={busy === 'answer'}
                    disabled={!text.trim() && choice === ''}
                  >
                    Send answer
                  </Button>
                </Box>
              </Card>
            )}

            {it.kind === 'permission' && (
              <Card my="3">
                <Text as="div" mb="2">
                  The agent wants to use <Code>{str(p.tool_name)}</Code>:
                </Text>
                <Pre>{str(p)}</Pre>
                <Flex gap="2" mt="3">
                  <Button onClick={() => void send('allow')} loading={busy === 'allow'}>
                    Allow
                  </Button>
                  <Button color="red" variant="soft" onClick={() => void send('deny')} loading={busy === 'deny'}>
                    Deny
                  </Button>
                </Flex>
              </Card>
            )}

            {it.kind === 'admin_change' && (
              <Card my="3">
                <Text as="div" weight="medium" mb="2" className="fleet-wrap">
                  {str(p.summary)}
                </Text>
                {typeof p.note === 'string' && (
                  <Text as="p" size="2" color="gray" mb="2" className="fleet-wrap">
                    The orchestrator’s note, in its own words: {p.note}
                  </Text>
                )}
                <Text as="div" size="1" color="gray" mb="1">
                  The exact change
                </Text>
                <Pre>{str(p.action)}</Pre>
                <Flex gap="2" mt="3">
                  <Button onClick={() => void send('accept')} loading={busy === 'accept'}>
                    Approve
                  </Button>
                  <Button variant="soft" color="red" onClick={() => void send('dismiss')} loading={busy === 'dismiss'}>
                    Reject
                  </Button>
                </Flex>
              </Card>
            )}

            {(it.kind === 'task_proposal' || it.kind === 'slack_reply' || it.kind === 'reply') && (
              <Card my="3">
                {it.kind === 'task_proposal' ? (
                  <>
                    <Text as="div" weight="medium" className="fleet-wrap">
                      {str(p.title)}
                    </Text>
                    <Text as="div" size="2" color="gray" mb="2">
                      in {str(p.project)}
                    </Text>
                    <Pre>{str(p.brief)}</Pre>
                  </>
                ) : (
                  <>
                    <Text as="div" size="2" mb="2" className="fleet-wrap">
                      Reply to <strong>{str(p.destination ?? p.channel_id)}</strong>, posted{' '}
                      <strong>as you</strong>
                      {typeof p.in_reply_to === 'string' ? `, answering ${p.in_reply_to}` : ''}.
                    </Text>
                    <Pre>{str(p.text)}</Pre>
                    {typeof p.quote === 'string' && p.quote && (
                      <Text as="p" size="1" color="gray" mt="2" className="fleet-wrap">
                        {p.quote}
                      </Text>
                    )}
                    <Text as="p" size="1" color="gray" mt="2">
                      To change the wording, reply to the draft in Telegram.
                    </Text>
                  </>
                )}
                <Flex gap="2" mt="3">
                  <Button onClick={() => void send('accept')} loading={busy === 'accept'}>
                    {it.kind === 'task_proposal' ? 'Create the task' : 'Send'}
                  </Button>
                  <Button variant="soft" onClick={() => void send('dismiss')} loading={busy === 'dismiss'}>
                    Dismiss
                  </Button>
                </Flex>
              </Card>
            )}

            {/* A kind this page has no form for: it is still shown, as text, and answered in
                Telegram. Better than a page that looks empty. */}
            {!WITH_FORM.includes(it.kind) && (
              <Card my="3">
                <Text as="div" size="2" color="gray" mb="2">
                  {it.summary || 'The agent is waiting on you.'}
                </Text>
                <Pre>{str(p)}</Pre>
                <Text as="p" size="1" color="gray" mt="2">
                  Answer this one in Telegram.
                </Text>
              </Card>
            )}

            {plan && (
              <Card my="3">
                <Heading size="3" mb="2">
                  Decision
                </Heading>
                <Text as="div" size="2" mb="1">
                  Feedback — required to ask for changes
                </Text>
                <TextArea value={feedback} onChange={(e) => setFeedback(e.currentTarget.value)} />
                <Flex gap="2" mt="3">
                  <Button onClick={() => void send('approve')} loading={busy === 'approve'}>
                    Approve the plan
                  </Button>
                  <Button
                    variant="soft"
                    onClick={() => void send('revise', { feedback })}
                    loading={busy === 'revise'}
                    disabled={!feedback.trim()}
                  >
                    Request changes
                  </Button>
                </Flex>
              </Card>
            )}
          </>
        )
      }}
    </Loaded>
  )
}
