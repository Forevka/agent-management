import { useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardFooter, CardHeader } from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import { TextArea } from '@/components/ui/text-area'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Note } from '@/components/fx/note'
import { RadioGroup } from '@/components/fx/radio'
import * as api from '../api'
import { HOLDS_THE_AGENT } from '../api'
import type { Item as ItemT } from '../api'
import { ErrorNote, Loaded, Pre, useApi } from '../ui'

const str = (v: unknown): string =>
  typeof v === 'string' ? v : v === undefined || v === null ? '' : JSON.stringify(v, null, 2)

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
          const next = multi ? (on ? [...picked, label] : picked.filter((p) => p !== label)) : [label]
          set({ ...answers, [item.id]: { ...(answers[item.id] ?? {}), [text]: next } })
        }
        return (
          <Card key={qi}>
            <CardHeader title={text} titleAs="h3" />
            <CardContent>
              {multi ? (
                <Stack gap="2">
                  {options.map((o, oi) => (
                    <Checkbox
                      key={oi}
                      variant="card"
                      label={str(o.label)}
                      description={str(o.description) || undefined}
                      checked={picked.includes(str(o.label))}
                      onChange={(e) => pick(str(o.label), e.currentTarget.checked)}
                    />
                  ))}
                </Stack>
              ) : (
                <RadioGroup
                  name={`q-${item.id}-${qi}`}
                  value={picked[0] ?? ''}
                  onChange={(v) => pick(v, true)}
                  options={options.map((o) => ({
                    value: str(o.label),
                    label: str(o.label),
                    description: str(o.description) || undefined,
                  }))}
                />
              )}
            </CardContent>
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
      <Page>
        <Note tone="success" title={done.join(' · ') || 'Answered.'}>
          <Link to="/" className="fx-link">
            Back to the dashboard
          </Link>
        </Note>
      </Page>
    )
  }

  return (
    <Page>
      <Loaded state={state}>
        {(it) => {
          const p = it.payload ?? {}
          const plan = it.kind === 'plan' ? it : it.related?.find((r) => r.kind === 'plan')
          const questions = [it, ...(it.related ?? [])].filter((i) => i.kind === 'question')
          const waiting = HOLDS_THE_AGENT.includes(it.kind)
            ? 'the agent is stopped until you answer.'
            : it.kind === 'ask'
              ? 'your answer reaches the agent as a message; it keeps working meanwhile.'
              : ''
          return (
            <>
              <PageHeader
                title={plan ? 'Plan review' : `Q-${it.id}`}
                // What is waiting, in words, so the question can be answered from this page alone.
                description={
                  it.task_title || waiting
                    ? `${it.task_title ?? ''}${it.task_title && waiting ? ' — ' : ''}${waiting}`
                    : undefined
                }
                meta={
                  <Row gap="2">
                    <Badge tone={it.blocking ? 'danger' : 'warning'} dot>
                      {it.kind}
                    </Badge>
                    {it.task && (
                      <Link to={`/tasks/${it.task}`} className="fx-link">
                        {it.task}
                      </Link>
                    )}
                    {it.agent && (
                      <Text size="1" tone="muted">
                        {it.agent}
                      </Text>
                    )}
                  </Row>
                }
              />

              {it.state && it.state !== 'open' && <Note tone="info">This was already {it.state}.</Note>}

              {error && <ErrorNote>{error}</ErrorNote>}

              {/* A plan is shown as text, not rendered markup: an agent writes it. */}
              {plan && (
                <Section title="The agent’s plan">
                  <Pre>{str(plan.payload?.plan ?? plan.summary)}</Pre>
                </Section>
              )}

              {questions.length > 0 && (
                <Section title="Questions">
                  <Stack gap="2">
                    {questions.map((q) => (
                      <Questions key={q.id} item={q} answers={answers} set={setAnswers} />
                    ))}
                    {!plan && (
                      <Row>
                        <Button onClick={() => void send('questions')} loading={busy === 'questions'}>
                          Submit answers
                        </Button>
                      </Row>
                    )}
                  </Stack>
                </Section>
              )}

              {it.kind === 'ask' && (
                <Card>
                  <CardHeader title={str(p.question) || it.summary} />
                  <CardContent>
                    <Stack gap="3">
                      {((p.options as unknown[]) ?? []).length > 0 && (
                        <RadioGroup
                          name={`ask-${it.id}`}
                          value={choice}
                          onChange={setChoice}
                          options={((p.options as unknown[]) ?? []).map((o, i) => ({
                            value: String(i),
                            label: str(o),
                          }))}
                        />
                      )}
                      <TextArea
                        label="Or answer in your own words"
                        value={text}
                        onChange={(e) => setText(e.currentTarget.value)}
                        minRows={2}
                      />
                    </Stack>
                  </CardContent>
                  <CardFooter>
                    <Button
                      onClick={() => void send('answer', text.trim() ? { text } : { choice: Number(choice) })}
                      loading={busy === 'answer'}
                      disabled={!text.trim() && choice === ''}
                    >
                      Send answer
                    </Button>
                  </CardFooter>
                </Card>
              )}

              {it.kind === 'permission' && (
                <Card>
                  <CardHeader title={`The agent wants to use ${str(p.tool_name)}`} />
                  <CardContent>
                    <Pre>{str(p)}</Pre>
                  </CardContent>
                  <CardFooter>
                    <Row gap="2">
                      <Button onClick={() => void send('allow')} loading={busy === 'allow'}>
                        Allow
                      </Button>
                      <Button variant="destructive" onClick={() => void send('deny')} loading={busy === 'deny'}>
                        Deny
                      </Button>
                    </Row>
                  </CardFooter>
                </Card>
              )}

              {it.kind === 'admin_change' && (
                <Card>
                  <CardHeader
                    title={str(p.summary)}
                    description={
                      typeof p.note === 'string'
                        ? `The orchestrator’s note, in its own words: ${p.note}`
                        : undefined
                    }
                  />
                  <CardContent>
                    <Stack gap="2">
                      <Text size="1" tone="muted">
                        The exact change
                      </Text>
                      <Pre>{str(p.action)}</Pre>
                    </Stack>
                  </CardContent>
                  <CardFooter>
                    <Row gap="2">
                      <Button onClick={() => void send('accept')} loading={busy === 'accept'}>
                        Approve
                      </Button>
                      <Button variant="destructive" onClick={() => void send('dismiss')} loading={busy === 'dismiss'}>
                        Reject
                      </Button>
                    </Row>
                  </CardFooter>
                </Card>
              )}

              {(it.kind === 'task_proposal' || it.kind === 'slack_reply' || it.kind === 'reply') && (
                <Card>
                  {it.kind === 'task_proposal' ? (
                    <>
                      <CardHeader title={str(p.title)} description={`in ${str(p.project)}`} />
                      <CardContent>
                        <Pre>{str(p.brief)}</Pre>
                      </CardContent>
                    </>
                  ) : (
                    <>
                      <CardHeader
                        title={`Reply to ${str(p.destination ?? p.channel_id)}`}
                        description={`Posted as you${typeof p.in_reply_to === 'string' ? `, answering ${p.in_reply_to}` : ''}.`}
                      />
                      <CardContent>
                        <Stack gap="2">
                          <Pre>{str(p.text)}</Pre>
                          {typeof p.quote === 'string' && p.quote && (
                            <Text size="1" tone="muted" breakAll>
                              {p.quote}
                            </Text>
                          )}
                          <Text size="1" tone="muted">
                            To change the wording, reply to the draft in Telegram.
                          </Text>
                        </Stack>
                      </CardContent>
                    </>
                  )}
                  <CardFooter>
                    <Row gap="2">
                      <Button onClick={() => void send('accept')} loading={busy === 'accept'}>
                        {it.kind === 'task_proposal' ? 'Create the task' : 'Send'}
                      </Button>
                      <Button variant="secondary" onClick={() => void send('dismiss')} loading={busy === 'dismiss'}>
                        Dismiss
                      </Button>
                    </Row>
                  </CardFooter>
                </Card>
              )}

              {/* A kind this page has no form for: it is still shown, as text, and answered in
                  Telegram. Better than a page that looks empty. */}
              {!WITH_FORM.includes(it.kind) && (
                <Card>
                  <CardHeader
                    title={it.summary || 'The agent is waiting on you.'}
                    description="Answer this one in Telegram."
                  />
                  <CardContent>
                    <Pre>{str(p)}</Pre>
                  </CardContent>
                </Card>
              )}

              {plan && (
                <Card>
                  <CardHeader title="Decision" />
                  <CardContent>
                    <TextArea
                      label="Feedback"
                      hint="Required to ask for changes."
                      value={feedback}
                      onChange={(e) => setFeedback(e.currentTarget.value)}
                      minRows={2}
                    />
                  </CardContent>
                  <CardFooter>
                    <Row gap="2">
                      <Button onClick={() => void send('approve')} loading={busy === 'approve'}>
                        Approve the plan
                      </Button>
                      <Button
                        variant="secondary"
                        onClick={() => void send('revise', { feedback })}
                        loading={busy === 'revise'}
                        disabled={!feedback.trim()}
                      >
                        Request changes
                      </Button>
                    </Row>
                  </CardFooter>
                </Card>
              )}
            </>
          )
        }}
      </Loaded>
    </Page>
  )
}
