import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { TextArea } from '@/components/ui/text-area'
import { Page, PageHeader, Row, Stack, Text } from '@/components/fx/layout'
import { ArrowLeftIcon } from '@/components/fx/icons'
import {
  LiveBadge,
  SessionEntry,
  SessionPartial,
  SessionStyles,
  SessionWorking,
  buildEntries,
  type Line,
  type Partial,
} from '@/components/fx/session'
import * as api from '../api'
import type { Events, TaskDetail as Detail } from '../api'
import { ActionButton, Loaded, StatusBadge, useApi } from '../ui'

/// The fleet's standing sessions, and who may be written to. The owner talks to the brain; the
/// secretary and the retrospective take nobody's word here (the server refuses it too).
const NAMES: Record<string, string> = { brain: 'Brain', secretary: 'Secretary', retro: 'Retrospective' }
const WRITABLE_BUILTIN = ['brain']
const FINISHED = ['done', 'stopped', 'failed']

/// How close to the bottom counts as "following": new lines scroll into view only then, so
/// someone reading further up is not pulled away.
const STICK_PX = 160

/// A message the owner sent that the transcript does not show yet: it appears at once, and goes
/// when its `in` line arrives (or, kept for later, says so).
type Pending = { id: number; text: string; queued: boolean }

/// One session, live: its conversation as it happens, and a box to continue it. The page loads
/// the newest lines, then follows the server-sent stream from there (`/api/tasks/{id}/stream`):
/// each recorded line, and what the run is writing before it finishes (never stored).
export default function Session() {
  const { id = '' } = useParams()
  // A page of its own per task: moving from one session to another starts from nothing rather
  // than carrying the last one's position into the new stream.
  return <SessionView key={id} id={id} />
}

/// After a stream closes for good (a 502 while the orchestrator restarts for a deploy), how long
/// before the page opens a new one.
const REOPEN_MS = 3_000

function SessionView({ id }: { id: string }) {
  const path = `/api/tasks/${encodeURIComponent(id)}`
  // The task's status, items and run change slowly; the stream carries the conversation.
  const detail = useApi<Detail>(path, 5_000)
  const [lines, setLines] = useState<Line[] | null>(null)
  const [more, setMore] = useState(false)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [partial, setPartial] = useState<Partial | null>(null)
  const [link, setLink] = useState<'live' | 'connecting' | 'off'>('off')
  const [pending, setPending] = useState<Pending[]>([])
  const [attempt, setAttempt] = useState(0)
  // Every line id the page holds: the stream looks back a little on every read (lines can be
  // committed out of order) and resumes from an older id after a reconnect, so it repeats some.
  const seen = useRef(new Set<number>())
  const newest = useRef(0)
  // The newest finished output line: a partial from before it is about a block already shown.
  const lastOut = useRef(0)
  const bottom = useRef<HTMLDivElement>(null)
  const stick = useRef(true)

  const take = (incoming: Line[]) => {
    const fresh = incoming.filter((l) => !seen.current.has(l.id))
    if (!fresh.length) return
    for (const l of fresh) {
      seen.current.add(l.id)
      newest.current = Math.max(newest.current, l.id)
      if (l.kind === 'out' || l.kind === 'exit') lastOut.current = Math.max(lastOut.current, l.id)
    }
    setLines((ls) => {
      const all = [...(ls ?? []), ...fresh]
      // Almost always already in order; a late line goes where its id puts it.
      return all.some((l, i) => i > 0 && all[i - 1].id > l.id) ? all.sort((a, b) => a.id - b.id) : all
    })
  }

  // The newest lines first; the stream continues from the last of them.
  useEffect(() => {
    let gone = false
    api
      .get<Events>(`${path}/events?limit=300`)
      .then((e) => {
        if (gone) return
        take(e.events)
        setLines((ls) => ls ?? [])
        setMore(e.more)
      })
      .catch((e: unknown) => !gone && setLoadError(e instanceof Error ? e.message : String(e)))
    return () => {
      gone = true
    }
    // `take` only touches refs and state setters, so it is not a dependency.
  }, [path])

  const ready = lines !== null
  useEffect(() => {
    if (!ready) return
    // The browser reconnects by itself and says where it was (`Last-Event-ID`); `after` is
    // where a new stream starts. One that closed for good (the server answered an error, as
    // it does for a moment during a deploy) is opened again after a pause.
    const es = new EventSource(`${path}/stream?after=${newest.current}`)
    let reopen: ReturnType<typeof setTimeout> | undefined
    setLink('connecting')
    es.onopen = () => setLink('live')
    es.onerror = () => {
      if (es.readyState === EventSource.CLOSED) {
        setLink('connecting')
        reopen = setTimeout(() => setAttempt((a) => a + 1), REOPEN_MS)
      } else {
        setLink('connecting')
      }
    }
    es.addEventListener('line', (m) => {
      const line = JSON.parse((m as MessageEvent).data) as Line
      if (seen.current.has(line.id)) return
      take([line])
      // A finished line supersedes whatever was being written.
      if (line.kind === 'out' || line.kind === 'exit') setPartial(null)
      if (line.kind === 'in') setPending((ps) => ps.filter((p) => !line.data.includes(p.text)))
    })
    es.addEventListener('partial', (m) => {
      const p = JSON.parse((m as MessageEvent).data) as Partial & { after: number }
      if (p.after >= lastOut.current) setPartial(p)
    })
    return () => {
      clearTimeout(reopen)
      es.close()
      setLink('off')
    }
  }, [path, ready, attempt])

  const entries = useMemo(() => buildEntries(lines ?? []), [lines])

  // Follow the bottom while the reader is there. The page scrolls inside the app shell's panel,
  // not the window, and scroll events do not bubble: so listen in the capture phase and look at
  // whichever scroller holds the transcript.
  useEffect(() => {
    const onScroll = (e: Event) => {
      const el = e.target
      if (!(el instanceof HTMLElement) || !bottom.current || !el.contains(bottom.current)) return
      stick.current = el.scrollTop + el.clientHeight >= el.scrollHeight - STICK_PX
    }
    document.addEventListener('scroll', onScroll, { capture: true, passive: true })
    return () => document.removeEventListener('scroll', onScroll, { capture: true })
  }, [])
  useLayoutEffect(() => {
    if (stick.current) bottom.current?.scrollIntoView({ block: 'end' })
  }, [entries, partial, pending])

  const earlier = () => {
    const first = lines?.[0]?.id
    if (!first) return
    stick.current = false
    void api.get<Events>(`${path}/events?limit=300&before=${first}`).then((e) => {
      take(e.events)
      setMore(e.more)
    })
  }

  return (
    <Page>
      <SessionStyles />
      <style href="fx-session-page" precedence="default">
        {pageCss}
      </style>
      <Loaded state={detail}>
        {(d) => {
          const t = d.task
          const builtin = t.id in NAMES
          const agent = NAMES[t.id] ?? t.agent ?? 'Agent'
          const writable = !builtin || WRITABLE_BUILTIN.includes(t.id)
          const open = d.items.filter((i) => i.state === 'open')
          const last = entries.at(-1)
          // Mid-turn with nothing streaming: something is happening that has not printed yet.
          const working = t.running && !partial && last?.t !== 'turn' && last?.t !== 'note'
          return (
            <>
              <PageHeader
                title={builtin ? NAMES[t.id] : t.key}
                description={builtin ? 'The fleet’s own standing session.' : t.title}
                back={
                  <Link to={builtin ? '/sessions' : `/tasks/${encodeURIComponent(t.id)}`} className="fx-link" data-muted="">
                    <ArrowLeftIcon /> {builtin ? 'Sessions' : 'Task'}
                  </Link>
                }
                meta={
                  <Row gap="2">
                    <StatusBadge status={t.status} running={t.running} />
                    <LiveBadge state={link} />
                    {t.node && (
                      <Text size="1" tone="muted">
                        on {t.node}
                      </Text>
                    )}
                  </Row>
                }
                action={
                  !builtin && (t.running || !FINISHED.includes(t.status)) ? (
                    <ActionButton
                      path={path}
                      op="stop"
                      variant="destructive"
                      confirm={`Stop ${t.key}? Its agent is stopped now; a message resumes the task later.`}
                      onDone={() => void detail.reload()}
                    >
                      Stop
                    </ActionButton>
                  ) : undefined
                }
              />

              <div className="fx-session">
                {loadError && (
                  <Text tone="danger" size="1">
                    {loadError}
                  </Text>
                )}
                {more && (
                  <Button size="sm" variant="ghost" onClick={earlier}>
                    Load earlier
                  </Button>
                )}
                {lines !== null && entries.length === 0 && !partial && (
                  <Text tone="muted" size="2">
                    Nothing has happened in this session yet.
                  </Text>
                )}
                {entries.map((e) => (
                  <SessionEntry key={e.key} e={e} live={t.running} agent={agent} />
                ))}
                {partial && <SessionPartial p={partial} agent={agent} />}
                {pending.map((p) => (
                  <div key={p.id} className="fx-session__in" data-who="owner">
                    <div className="fx-session__who">
                      You{' '}
                      <span className="fx-session__muted">
                        {p.queued ? 'kept for when a node can take it' : 'sending…'}
                      </span>
                    </div>
                    <div className="fx-session__bubble fx-session__plain" data-pending="">
                      {p.text}
                    </div>
                  </div>
                ))}
                {working && <SessionWorking />}
                {open.length > 0 && (
                  <Stack gap="2">
                    {open.map((i) => (
                      <Link key={i.id} to={`/items/${i.id}`} className="fx-sessionpage__item">
                        <Badge tone={i.blocking ? 'danger' : 'warning'} dot>
                          {i.kind}
                        </Badge>
                        <Text size="2" breakAll>
                          {i.summary}
                        </Text>
                        <Text size="1" tone="muted">
                          Answer →
                        </Text>
                      </Link>
                    ))}
                  </Stack>
                )}
                <div ref={bottom} />
              </div>

              {writable ? (
                <Composer
                  path={path}
                  finished={FINISHED.includes(t.status) && !t.running}
                  onSending={(p) => setPending((ps) => [...ps, p])}
                  onSent={(pid, queued) => {
                    setPending((ps) => (queued ? ps.map((p) => (p.id === pid ? { ...p, queued } : p)) : ps))
                    stick.current = true
                  }}
                  onFailed={(pid) => setPending((ps) => ps.filter((p) => p.id !== pid))}
                />
              ) : (
                <Text size="1" tone="muted" as="p">
                  {NAMES[t.id]} takes no messages from here: it reads what the fleet hands it.
                </Text>
              )}
            </>
          )
        }}
      </Loaded>
    </Page>
  )
}

let nextPending = 1

/// The box at the bottom. Enter sends (Shift+Enter is a new line), as in a chat; the text is
/// shown at once and the transcript confirms it when the session takes it.
function Composer({
  path,
  finished,
  onSending,
  onSent,
  onFailed,
}: {
  path: string
  finished: boolean
  onSending: (p: Pending) => void
  onSent: (id: number, queued: boolean) => void
  onFailed: (id: number) => void
}) {
  const [text, setText] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const inFlight = useRef(false)
  const send = () => {
    const body = text.trim()
    if (!body || inFlight.current) return
    inFlight.current = true
    setBusy(true)
    setError(null)
    const pid = nextPending++
    onSending({ id: pid, text: body, queued: false })
    setText('')
    api
      .act<{ delivered: 'to_run' | 'queued' }>(path, 'send', { text: body })
      .then((r) => onSent(pid, r.delivered === 'queued'))
      .catch((e: unknown) => {
        onFailed(pid)
        setText(body)
        setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        inFlight.current = false
        setBusy(false)
      })
  }
  return (
    <div className="fx-sessionpage__composer">
      <form
        onSubmit={(e) => {
          e.preventDefault()
          send()
        }}
      >
        <Row gap="2" align="end" wrap={false}>
          <div style={{ flex: 1, minWidth: 0 }}>
            <TextArea
              aria-label="Message"
              placeholder={finished ? 'Continue this session…' : 'Message the agent…'}
              value={text}
              minRows={1}
              maxRows={10}
              onChange={(e) => setText(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
                  e.preventDefault()
                  send()
                }
              }}
            />
          </div>
          <Button type="submit" loading={busy} disabled={!text.trim()}>
            Send
          </Button>
        </Row>
      </form>
      {error && (
        <Text size="1" tone="danger">
          {error}
        </Text>
      )}
    </div>
  )
}

const pageCss = /* css */ `
.fx-sessionpage__composer {
  position: sticky;
  bottom: 0;
  z-index: 2;
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 0 -4px;
  padding: 12px 4px 16px;
  /* The page panel of the app shell is the surface colour, so the composer is too. */
  background: var(--aui-surface, #fff);
  border-top: 1px solid var(--aui-border, #e5e5e5);
}
.fx-sessionpage__item {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 12px;
  border: 1px solid var(--aui-border, #e5e5e5);
  border-radius: var(--aui-radius, 10px);
  background: var(--aui-surface, #fff);
  color: inherit;
  text-decoration: none;
  min-width: 0;
}
.fx-sessionpage__item:hover { border-color: var(--aui-border-strong, #d4d4d8); }
.fx-session__bubble[data-pending] { opacity: 0.7; }
`
