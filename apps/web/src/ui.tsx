// The pieces every page is built from: how data is loaded, how a status reads, how time reads.
//
// Nothing here styles anything. The look lives in the components under `components/ui` (affectUI,
// MIT) and `components/fx` (ours, same contract); this file is the app's own vocabulary on top of
// them — see apps/web/DESIGN.md.

import { useCallback, useEffect, useRef, useState } from 'react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader } from '@/components/ui/card'
import { CodeBlock } from '@/components/ui/code-block'
import { Spinner } from '@/components/ui/spinner'
import { Note } from '@/components/fx/note'
import { Text } from '@/components/fx/layout'
import * as api from './api'

/// Loading, with three outcomes the pages treat differently: data, an error worth showing, and
/// "the session is gone", which the shell turns into a sign-in screen rather than an error.
export function useApi<T>(path: string | null, intervalMs?: number) {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [expired, setExpired] = useState(false)
  const [loading, setLoading] = useState(path !== null)
  // Kept in a ref so the polling effect does not restart on every render.
  const seq = useRef(0)

  const reload = useCallback(async () => {
    if (path === null) return
    const mine = ++seq.current
    try {
      const next = await api.get<T>(path)
      // A slow earlier request must not overwrite a newer answer.
      if (mine === seq.current) {
        setData(next)
        setError(null)
      }
    } catch (e) {
      if (e instanceof api.Unauthorized) setExpired(true)
      else if (mine === seq.current) setError(e instanceof Error ? e.message : String(e))
    } finally {
      if (mine === seq.current) setLoading(false)
    }
  }, [path])

  useEffect(() => {
    void reload()
    if (!intervalMs) return
    const t = setInterval(() => void reload(), intervalMs)
    return () => clearInterval(t)
  }, [reload, intervalMs])

  return { data, error, loading, expired, reload }
}

/// A page's shared loading and error handling, so no page reimplements it.
export function Loaded<T>({
  state,
  children,
}: {
  state: ReturnType<typeof useApi<T>>
  children: (data: T) => React.ReactNode
}) {
  if (state.expired) return <SignIn />
  if (state.error) return <ErrorNote>{state.error}</ErrorNote>
  if (!state.data) {
    return (
      <div
        style={{ display: 'flex', alignItems: 'center', gap: 8, justifyContent: 'center', padding: '48px 0' }}
        role="status"
      >
        <Spinner size={16} />
        <Text tone="muted">Loading…</Text>
      </div>
    )
  }
  return <>{children(state.data)}</>
}

export function ErrorNote({ children }: { children: React.ReactNode }) {
  return (
    <Note tone="danger" title="That did not work">
      {children}
    </Note>
  )
}

export function SignIn() {
  return (
    <Card>
      <CardHeader title="Sign in" description="This page needs a fresh link." />
      <CardContent>
        <Text as="p" tone="muted">
          Send <Text mono>/web</Text> to the orchestrator bot in Telegram and open the link it sends
          back.
        </Text>
      </CardContent>
    </Card>
  )
}

export function Empty({ children }: { children: React.ReactNode }) {
  return (
    <Text as="p" tone="muted" size="1">
      {children}
    </Text>
  )
}

/// Status tones, one mapping for the whole app so a word always looks the same.
const TASK_TONES: Record<string, 'neutral' | 'info' | 'success' | 'warning' | 'danger' | 'purple'> = {
  running: 'success',
  planning: 'purple',
  waiting: 'warning',
  queued: 'info',
  done: 'neutral',
  stopped: 'neutral',
  failed: 'danger',
  // Done and put away by the nightly archive: as quiet as done.
  archived: 'neutral',
}

/// A task's status and whether a session is live are two different things, and the orchestrator's
/// word for the first is "running" either way: a task whose agent finished its turn keeps
/// `status = running` (it is in progress and resumable) with no session behind it. Showing that
/// as "running" says something untrue, so an in-progress task with nothing live reads as **idle**
/// — the work is open, nothing is happening this second. A live session gets the pulsing dot,
/// which is the one place in this app something blinks.
export function StatusBadge({ status, running }: { status: string; running?: boolean }) {
  const resting = !running && (status === 'running' || status === 'planning')
  return (
    <span style={{ display: 'inline-flex', gap: 6, alignItems: 'center', flexWrap: 'wrap' }}>
      <Badge
        tone={resting ? 'neutral' : (TASK_TONES[status] ?? 'neutral')}
        dot
        title={
          resting
            ? `${status}, but no session is live: the work is open and a message resumes it`
            : undefined
        }
      >
        {resting ? 'idle' : status}
      </Badge>
      {running && (
        <Badge tone="success" live title="a session is running right now">
          live
        </Badge>
      )}
    </span>
  )
}

/// Time as a person reads it ("4 min ago"), with the exact timestamp on hover — the absolute
/// one still matters when something went wrong.
export function When({ at }: { at: string | null | undefined }) {
  if (!at) return <Text tone="muted">—</Text>
  const d = new Date(at)
  const secs = Math.round((Date.now() - d.getTime()) / 1000)
  const rel = (): string => {
    const abs = Math.abs(secs)
    if (abs < 45) return 'just now'
    const steps: [number, string][] = [
      [60, 'min'],
      [3600, 'h'],
      [86400, 'd'],
    ]
    if (abs < 3600) return `${Math.round(abs / steps[0][0])} ${steps[0][1]}${secs < 0 ? ' from now' : ' ago'}`
    if (abs < 86400) return `${Math.round(abs / steps[1][0])}${steps[1][1]}${secs < 0 ? ' from now' : ' ago'}`
    if (abs < 86400 * 30) return `${Math.round(abs / steps[2][0])}${steps[2][1]}${secs < 0 ? ' from now' : ' ago'}`
    return d.toLocaleDateString()
  }
  return (
    <Text size="1" tone="muted" title={d.toLocaleString()}>
      {rel()}
    </Text>
  )
}

/// How long a session lasted, or how long it has been going.
export function Duration({ from, to }: { from: string; to: string | null }) {
  const end = to ? new Date(to).getTime() : Date.now()
  const secs = Math.max(0, Math.round((end - new Date(from).getTime()) / 1000))
  const text =
    secs < 60 ? `${secs}s` : secs < 3600 ? `${Math.round(secs / 60)}m` : `${(secs / 3600).toFixed(1)}h`
  return (
    <Text size="1" tone={to ? 'muted' : 'success'}>
      {text}
      {!to && ' …'}
    </Text>
  )
}

/// A button that runs one action, shows that it is running, and surfaces what came back. Every
/// mutating control in the app is one of these, so none of them can be double-submitted.
export function ActionButton({
  op,
  path,
  body,
  children,
  variant = 'secondary',
  size,
  confirm,
  onDone,
  disabled,
}: {
  op: string
  path: string
  body?: unknown
  children: React.ReactNode
  variant?: 'primary' | 'secondary' | 'ghost' | 'destructive'
  size?: 'sm' | 'md' | 'lg'
  /// When set, the click asks first. For anything that cannot be undone.
  confirm?: string
  onDone?: (result: { done?: unknown }) => void
  disabled?: boolean
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const run = async () => {
    if (confirm && !window.confirm(confirm)) return
    setBusy(true)
    setError(null)
    try {
      const r = await api.act(path, op, body ?? {})
      onDone?.(r)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }
  return (
    <span style={{ display: 'inline-flex', flexDirection: 'column', gap: 4, minWidth: 0 }}>
      <Button
        onClick={() => void run()}
        variant={variant}
        size={size ?? 'sm'}
        disabled={busy || disabled}
        loading={busy}
      >
        {children}
      </Button>
      {error && (
        <Text size="1" tone="danger">
          {error}
        </Text>
      )}
    </span>
  )
}

/// Agent output, a brief, a ticket: untrusted text, shown as text, never as markup. The code
/// block wraps, scrolls and can be copied, which is what someone reading a transcript wants.
export function Pre({ children, maxHeight = 352 }: { children: string; maxHeight?: number }) {
  return <CodeBlock code={children} wrap maxHeight={maxHeight} copyable />
}

export { Section } from '@/components/fx/layout'
