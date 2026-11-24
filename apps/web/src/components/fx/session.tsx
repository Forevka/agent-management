// A Claude Code session, read as the conversation it is: what was sent in, what the agent said,
// the tools it ran with their results folded under them, and where each turn ended. Built from
// the orchestrator's transcript lines (`task_events`): `in` is a text the fleet sent into the
// session, `out` one stream-json line of Claude's, the rest are the fleet's own notes.
//
// Everything here is untrusted text — an agent's words, a file it read, a Slack message in a
// batch — so it is rendered as text: the small Markdown reader below builds React elements,
// never markup.

import { useState, type ReactNode } from 'react'
import { Badge } from '@/components/ui/badge'
import { CodeBlock } from '@/components/ui/code-block'
import { ThinkingLoader } from '@/components/ui/thinking-loader'
import { ToolCallCard } from '@/components/ui/tool-call-card'

export type Line = { id: number; at: string; kind: string; data: string }

export type ToolResult = { text: string; error: boolean }

export type Entry =
  | { t: 'in'; key: string; at: string; text: string; who: 'owner' | 'fleet' }
  | { t: 'say'; key: string; at: string; text: string }
  | { t: 'think'; key: string; text: string }
  | { t: 'tool'; key: string; toolId: string; name: string; input: Record<string, unknown>; result?: ToolResult }
  | { t: 'turn'; key: string; ok: boolean; secs: number | null; cost: number | null }
  | { t: 'start'; key: string; at: string; model: string | null }
  | { t: 'note'; key: string; at: string; tone: 'muted' | 'warning' | 'danger'; text: string }

/// The live block a run is writing (never stored; see `live.rs`).
export type Partial = { run_id: string; kind: string; text: string }

const OWNER_PREFIXES = ['[Message from the human] ', '[Message from the owner] ']

type Json = Record<string, unknown>

const obj = (v: unknown): Json => (v && typeof v === 'object' && !Array.isArray(v) ? (v as Json) : {})
const str = (v: unknown): string => (typeof v === 'string' ? v : '')

/// A tool result's content: a string, or a list of blocks of which the text ones matter.
function resultText(content: unknown): string {
  if (typeof content === 'string') return content
  if (Array.isArray(content)) {
    return content
      .map((b) => {
        const o = obj(b)
        if (o.type === 'text') return str(o.text)
        if (o.type === 'image') return '[image]'
        return ''
      })
      .filter(Boolean)
      .join('\n')
  }
  return content == null ? '' : JSON.stringify(content)
}

/// Turns transcript lines into entries. Pure, so a page can rebuild it from all its lines on
/// every change (a session page holds a few hundred) and a tool result found later still lands
/// under its call.
export function buildEntries(lines: Line[]): Entry[] {
  const out: Entry[] = []
  const tools = new Map<string, number>()
  let stderr: { idx: number; lines: string[] } | null = null
  for (const l of lines) {
    if (l.kind !== 'err') stderr = null
    switch (l.kind) {
      case 'in': {
        // A run's first message is several texts joined (the brief, then what waited for the
        // run): each of the owner's reads as theirs, the rest as the fleet's.
        l.data.split(/\n\n(?=\[Message from the (?:human|owner)\] )/).forEach((part, i) => {
          const prefix = OWNER_PREFIXES.find((p) => part.startsWith(p))
          out.push({
            t: 'in',
            key: `${l.id}.${i}`,
            at: l.at,
            text: prefix ? part.slice(prefix.length) : part,
            who: prefix ? 'owner' : 'fleet',
          })
        })
        break
      }
      case 'out': {
        let v: Json
        try {
          v = obj(JSON.parse(l.data))
        } catch {
          out.push({ t: 'note', key: `${l.id}`, at: l.at, tone: 'muted', text: l.data })
          break
        }
        // A subagent's own traffic (the Task tool) stays inside its call.
        if (v.parent_tool_use_id) break
        if (v.type === 'system' && v.subtype === 'init') {
          out.push({ t: 'start', key: `${l.id}`, at: l.at, model: str(v.model) || null })
        } else if (v.type === 'assistant') {
          const blocks = obj(v.message).content
          ;(Array.isArray(blocks) ? blocks : []).forEach((b, i) => {
            const o = obj(b)
            const key = `${l.id}.${i}`
            if (o.type === 'text' && str(o.text).trim()) out.push({ t: 'say', key, at: l.at, text: str(o.text) })
            else if (o.type === 'thinking' && str(o.thinking).trim()) out.push({ t: 'think', key, text: str(o.thinking) })
            else if (o.type === 'tool_use') {
              tools.set(str(o.id), out.length)
              out.push({ t: 'tool', key, toolId: str(o.id), name: str(o.name), input: obj(o.input) })
            }
          })
        } else if (v.type === 'user') {
          const blocks = obj(v.message).content
          for (const b of Array.isArray(blocks) ? blocks : []) {
            const o = obj(b)
            if (o.type !== 'tool_result') continue
            const at = tools.get(str(o.tool_use_id))
            const result = { text: resultText(o.content), error: o.is_error === true }
            if (at !== undefined) {
              const e = out[at]
              if (e.t === 'tool') out[at] = { ...e, result }
            }
          }
        } else if (v.type === 'result') {
          const ms = typeof v.duration_ms === 'number' ? v.duration_ms : null
          out.push({
            t: 'turn',
            key: `${l.id}`,
            ok: v.subtype === 'success' && v.is_error !== true,
            secs: ms === null ? null : Math.round(ms / 1000),
            cost: typeof v.total_cost_usd === 'number' ? v.total_cost_usd : null,
          })
        }
        break
      }
      case 'err': {
        // Claude's stderr comes a line at a time; consecutive lines read as one block.
        if (stderr) {
          stderr.lines.push(l.data)
          const e = out[stderr.idx]
          if (e.t === 'note') out[stderr.idx] = { ...e, text: stderr.lines.join('\n') }
        } else {
          stderr = { idx: out.length, lines: [l.data] }
          out.push({ t: 'note', key: `${l.id}`, at: l.at, tone: 'danger', text: l.data })
        }
        break
      }
      case 'exit':
        out.push({
          t: 'note',
          key: `${l.id}`,
          at: l.at,
          tone: l.data === 'Some(0)' ? 'muted' : 'warning',
          text: l.data === 'Some(0)' ? 'Session closed' : `Session ended (${l.data.replace(/^Some\((.*)\)$/, 'exit $1')})`,
        })
        break
      case 'spawn_failed':
        out.push({ t: 'note', key: `${l.id}`, at: l.at, tone: 'danger', text: `Could not start: ${l.data}` })
        break
      case 'undelivered':
        out.push({ t: 'note', key: `${l.id}`, at: l.at, tone: 'muted', text: 'A message arrived after the session ended' })
        break
      case 'redelivered':
        out.push({ t: 'note', key: `${l.id}`, at: l.at, tone: 'muted', text: `It was ${l.data}` })
        break
      case 'notice':
      case 'waiting':
      case 'silent':
        out.push({ t: 'note', key: `${l.id}`, at: l.at, tone: 'warning', text: l.data })
        break
      // `owner` (the brain's copy of the owner's request, kept for habits), `settings` and
      // anything newer are bookkeeping, not part of the conversation.
      default:
        break
    }
  }
  return out
}

/// What a tool call is about, in a line: the command, the file, the pattern.
export function toolSummary(name: string, input: Record<string, unknown>): string {
  const pick = (...keys: string[]) => keys.map((k) => input[k]).find((v) => typeof v === 'string') as string | undefined
  const s =
    name === 'Bash'
      ? pick('command')
      : ['Read', 'Write', 'Edit', 'MultiEdit', 'NotebookEdit'].includes(name)
        ? pick('file_path', 'notebook_path')
        : ['Grep', 'Glob'].includes(name)
          ? pick('pattern')
          : name === 'WebFetch'
            ? pick('url')
            : name === 'WebSearch'
              ? pick('query')
              : ['Task', 'Agent'].includes(name)
                ? pick('description')
                : name === 'TodoWrite' && Array.isArray(input.todos)
                  ? `${input.todos.length} items`
                  : Object.values(input).find((v) => typeof v === 'string')
  return (s ?? '').split('\n')[0]
}

// ---------------------------------------------------------------- Markdown, as elements

/// `code`, **bold** and *emphasis* inside a line. Links stay as their text: a URL an agent
/// wrote is shown, not made clickable, for the same reason nothing here is markup.
function inline(text: string, key: string): ReactNode[] {
  const parts: ReactNode[] = []
  const re = /(`[^`\n]+`|\*\*[^*\n]+\*\*|\*[^*\n]+\*)/g
  let last = 0
  let m: RegExpExecArray | null
  let i = 0
  while ((m = re.exec(text))) {
    if (m.index > last) parts.push(text.slice(last, m.index))
    const t = m[0]
    const k = `${key}.${i++}`
    if (t.startsWith('`')) parts.push(<code key={k} className="fx-md__code">{t.slice(1, -1)}</code>)
    else if (t.startsWith('**')) parts.push(<strong key={k}>{t.slice(2, -2)}</strong>)
    else parts.push(<em key={k}>{t.slice(1, -1)}</em>)
    last = m.index + t.length
  }
  if (last < text.length) parts.push(text.slice(last))
  return parts
}

/// Enough Markdown for what agents write: paragraphs, headings, lists, fenced code and inline
/// code. Anything else reads fine as the text it is.
export function Markdown({ text }: { text: string }) {
  const blocks: ReactNode[] = []
  const lines = text.split('\n')
  let para: string[] = []
  let list: { ordered: boolean; items: string[] } | null = null
  const flushPara = () => {
    if (para.length) {
      const k = `p${blocks.length}`
      blocks.push(
        <p key={k} className="fx-md__p">
          {para.flatMap((l, i) => (i ? [<br key={`${k}.br${i}`} />, ...inline(l, `${k}.${i}`)] : inline(l, `${k}.${i}`)))}
        </p>,
      )
      para = []
    }
  }
  const flushList = () => {
    if (list) {
      const k = `l${blocks.length}`
      const items = list.items.map((it, i) => <li key={i}>{inline(it, `${k}.${i}`)}</li>)
      blocks.push(list.ordered ? <ol key={k} className="fx-md__list">{items}</ol> : <ul key={k} className="fx-md__list">{items}</ul>)
      list = null
    }
  }
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]
    const fence = line.match(/^\s*```(\w*)/)
    if (fence) {
      flushPara()
      flushList()
      const code: string[] = []
      i++
      while (i < lines.length && !/^\s*```/.test(lines[i])) code.push(lines[i++])
      blocks.push(
        <CodeBlock key={`c${blocks.length}`} code={code.join('\n')} language={fence[1] || undefined} wrap maxHeight={420} copyable />,
      )
      continue
    }
    const heading = line.match(/^(#{1,6})\s+(.*)$/)
    const bullet = line.match(/^\s*[-*+]\s+(.*)$/)
    const numbered = line.match(/^\s*\d+[.)]\s+(.*)$/)
    if (heading) {
      flushPara()
      flushList()
      blocks.push(
        <p key={`h${blocks.length}`} className="fx-md__h">
          {inline(heading[2], `h${blocks.length}`)}
        </p>,
      )
    } else if (bullet || numbered) {
      flushPara()
      const ordered = !bullet
      if (list && list.ordered !== ordered) flushList()
      list ??= { ordered, items: [] }
      list.items.push((bullet ?? numbered)![1])
    } else if (!line.trim()) {
      flushPara()
      flushList()
    } else {
      flushList()
      para.push(line)
    }
  }
  flushPara()
  flushList()
  return (
    <div className="fx-md">
      <style href="fx-md" precedence="default">
        {mdCss}
      </style>
      {blocks}
    </div>
  )
}

const mdCss = /* css */ `
.fx-md { display: flex; flex-direction: column; gap: 8px; min-width: 0; overflow-wrap: anywhere; }
.fx-md__p { margin: 0; }
.fx-md__h { margin: 4px 0 0; font-weight: 600; }
.fx-md__list { margin: 0; padding-left: 22px; display: flex; flex-direction: column; gap: 2px; }
.fx-md__code {
  font-family: var(--aui-font-mono);
  font-size: 0.92em;
  padding: 1px 5px;
  border-radius: 5px;
  background: var(--aui-subtle, #f0f0f0);
}
`

// ---------------------------------------------------------------- entries

/// A long text sent into a session (a brief, a batch of messages) folds after a few lines.
function Folded({ text, lines = 14 }: { text: string; lines?: number }) {
  const [open, setOpen] = useState(false)
  const all = text.split('\n')
  const long = all.length > lines || text.length > 1600
  const shown = open || !long ? text : all.slice(0, lines).join('\n').slice(0, 1600)
  return (
    <>
      <div className="fx-session__plain">{shown}{!open && long ? '…' : ''}</div>
      {long && (
        <button type="button" className="fx-session__more" onClick={() => setOpen(!open)}>
          {open ? 'Show less' : 'Show all'}
        </button>
      )}
    </>
  )
}

const toolName = (name: string) => name.replace(/^mcp__[^_]+__/, '')

/// A tool call, as affectUI's Tool Call Card: one line, unfolding to its input and result. A call
/// with no result in a session that is no longer running never got one (it was stopped or
/// lost), which is an error as far as the reader is concerned.
function Tool({ e, live }: { e: Extract<Entry, { t: 'tool' }>; live: boolean }) {
  const status = e.result ? (e.result.error ? 'error' : 'success') : live ? 'running' : 'error'
  return (
    <div className="fx-session__tool">
      <ToolCallCard
        name={toolName(e.name)}
        summary={toolSummary(e.name, e.input) || undefined}
        status={status}
        args={e.name === 'Bash' && typeof e.input.command === 'string' ? e.input.command : e.input}
        result={e.result && !e.result.error ? e.result.text || '(no output)' : undefined}
        error={
          e.result?.error ? e.result.text || 'failed' : !e.result && !live ? 'No result: the session ended first.' : undefined
        }
        openOnError={false}
        maxCodeHeight={360}
      />
    </div>
  )
}

function Thinking({ text, live }: { text: string; live?: boolean }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="fx-session__think">
      <button type="button" className="fx-session__more" aria-expanded={open} onClick={() => setOpen(!open)}>
        {live ? 'Thinking…' : 'Thought'} {open ? '▾' : '▸'}
      </button>
      {open && <div className="fx-session__plain fx-session__muted">{text}</div>}
    </div>
  )
}

function time(at: string) {
  return new Date(at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
}

/// One entry of the conversation. `live` says whether the session is running, which is what
/// makes a tool call without a result "running" rather than "never answered".
export function SessionEntry({ e, live, agent }: { e: Entry; live: boolean; agent: string }) {
  switch (e.t) {
    case 'in':
      return (
        <div className="fx-session__in" data-who={e.who}>
          <div className="fx-session__who">
            {e.who === 'owner' ? 'You' : 'The fleet'} <span className="fx-session__muted">{time(e.at)}</span>
          </div>
          <div className="fx-session__bubble">
            <Folded text={e.text} />
          </div>
        </div>
      )
    case 'say':
      return (
        <div className="fx-session__say">
          <div className="fx-session__who">
            {agent} <span className="fx-session__muted">{time(e.at)}</span>
          </div>
          <Markdown text={e.text} />
        </div>
      )
    case 'think':
      return <Thinking text={e.text} />
    case 'tool':
      return <Tool e={e} live={live} />
    case 'turn':
      return (
        <div className="fx-session__rule" data-tone={e.ok ? undefined : 'warning'}>
          <span>
            {e.ok ? 'Turn finished' : 'Turn ended with an error'}
            {e.secs !== null && ` · ${e.secs < 90 ? `${e.secs}s` : `${Math.round(e.secs / 60)} min`}`}
            {e.cost !== null && ` · $${e.cost.toFixed(2)}`}
          </span>
        </div>
      )
    case 'start':
      return (
        <div className="fx-session__rule">
          <span>
            Session started {time(e.at)}
            {e.model && ` · ${e.model}`}
          </span>
        </div>
      )
    case 'note':
      return (
        <div className="fx-session__note" data-tone={e.tone}>
          {e.tone === 'danger' ? <div className="fx-session__plain fx-session__mono">{e.text}</div> : e.text}
        </div>
      )
  }
}

/// What the run is writing this second: its words as they come, a tool call being written, or
/// thinking.
export function SessionPartial({ p, agent }: { p: Partial; agent: string }) {
  if (p.kind === 'tool') {
    return (
      <div className="fx-session__tool">
        <ToolCallCard name={toolName(p.text)} summary="writing the call…" status="running" />
      </div>
    )
  }
  if (p.kind === 'thinking') return <ThinkingLoader phases={['Thinking']} />
  return (
    <div className="fx-session__say" aria-live="polite">
      <div className="fx-session__who">{agent}</div>
      <div className="fx-session__streaming">
        <Markdown text={p.text} />
      </div>
    </div>
  )
}

export function SessionWorking() {
  return <ThinkingLoader phases={['Working']} />
}

export function SessionStyles() {
  return (
    <style href="fx-session" precedence="default">
      {css}
    </style>
  )
}

export function LiveBadge({ state }: { state: 'live' | 'connecting' | 'off' }) {
  if (state === 'live') {
    return (
      <Badge tone="success" live title="New lines appear as they are written">
        following
      </Badge>
    )
  }
  if (state === 'connecting') return <Badge tone="warning">reconnecting…</Badge>
  return null
}

const css = /* css */ `
.fx-session {
  --fx-session-line: var(--aui-border, #e5e5e5);
  display: flex;
  flex-direction: column;
  gap: 14px;
  min-width: 0;
}
.fx-session__who {
  font-size: 12px;
  font-weight: 600;
  color: var(--aui-ink, #171717);
  margin-bottom: 4px;
}
.fx-session__muted { color: var(--aui-muted, #737373); font-weight: 400; }
.fx-session__mono { font-family: var(--aui-font-mono); font-size: 12px; }
.fx-session__plain { white-space: pre-wrap; overflow-wrap: anywhere; min-width: 0; }
.fx-session__in { display: flex; flex-direction: column; align-items: flex-end; min-width: 0; }
.fx-session__in .fx-session__who { text-align: right; }
.fx-session__bubble {
  max-width: min(720px, 92%);
  padding: 10px 14px;
  border-radius: var(--aui-radius, 10px);
  background: var(--aui-surface, #fff);
  border: 1px solid var(--fx-session-line);
  box-shadow: var(--aui-shadow-card, none);
  min-width: 0;
}
.fx-session__in[data-who="owner"] .fx-session__bubble {
  background: var(--aui-primary, #171717);
  color: var(--aui-on-primary, #fff);
  border-color: transparent;
}
.fx-session__in[data-who="owner"] .fx-session__more { color: inherit; opacity: 0.8; }
.fx-session__say { min-width: 0; max-width: 820px; }
.fx-session__streaming .fx-md > :last-child::after {
  content: "";
  display: inline-block;
  width: 7px;
  height: 1em;
  margin-left: 2px;
  vertical-align: text-bottom;
  background: var(--aui-ink, #171717);
  animation: fx-session-caret 1s steps(2, start) infinite;
}
@keyframes fx-session-caret { to { visibility: hidden; } }
.fx-session__more {
  appearance: none;
  border: 0;
  background: none;
  padding: 0;
  margin-top: 4px;
  font: inherit;
  font-size: 12px;
  color: var(--aui-muted, #737373);
  cursor: pointer;
}
.fx-session__more:hover { text-decoration: underline; }
.fx-session__tool { min-width: 0; max-width: 820px; }
.fx-session__think { min-width: 0; max-width: 820px; }
.fx-session__rule {
  display: flex;
  align-items: center;
  gap: 10px;
  font-size: 12px;
  color: var(--aui-muted, #737373);
}
.fx-session__rule::before, .fx-session__rule::after {
  content: "";
  flex: 1;
  height: 1px;
  background: var(--fx-session-line);
}
.fx-session__rule[data-tone="warning"] { color: var(--aui-warning, #b25e00); }
.fx-session__note { font-size: 12.5px; color: var(--aui-muted, #737373); min-width: 0; }
.fx-session__note[data-tone="warning"] { color: var(--aui-warning, #b25e00); }
.fx-session__note[data-tone="danger"] {
  color: var(--aui-danger, #c4320a);
  background: var(--aui-danger-soft, #fdecea);
  border-radius: var(--aui-radius-sm, 7px);
  padding: 6px 10px;
}
@media (prefers-reduced-motion: reduce) {
  .fx-session__streaming .fx-md > :last-child::after { animation: none; }
}
`
