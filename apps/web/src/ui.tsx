// The pieces every page is built from: how data is loaded, how a status reads, how time reads,
// and the frame around it all.

import { useCallback, useEffect, useRef, useState } from 'react'
import {
  Badge, Box, Button, Callout, Card, Code, Flex, Heading, Spinner, Text,
} from '@radix-ui/themes'
import { ExclamationTriangleIcon } from '@radix-ui/react-icons'
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
      <Flex align="center" gap="2" py="6" justify="center">
        <Spinner />
        <Text color="gray">Loading…</Text>
      </Flex>
    )
  }
  return <>{children(state.data)}</>
}

export function ErrorNote({ children }: { children: React.ReactNode }) {
  return (
    <Callout.Root color="red" my="3">
      <Callout.Icon>
        <ExclamationTriangleIcon />
      </Callout.Icon>
      <Callout.Text>{children}</Callout.Text>
    </Callout.Root>
  )
}

export function SignIn() {
  return (
    <Card my="4">
      <Heading size="4" mb="2">
        Sign in
      </Heading>
      <Text as="p" color="gray">
        Send <Code>/web</Code> to the orchestrator bot in Telegram for a fresh login link.
      </Text>
    </Card>
  )
}

export function Empty({ children }: { children: React.ReactNode }) {
  return (
    <Box py="5">
      <Text color="gray" size="2">
        {children}
      </Text>
    </Box>
  )
}

/// Status colours, one mapping for the whole app so a word always looks the same.
const TASK_COLORS: Record<string, 'green' | 'blue' | 'amber' | 'gray' | 'red' | 'purple'> = {
  running: 'green',
  planning: 'purple',
  waiting: 'amber',
  queued: 'blue',
  done: 'gray',
  stopped: 'gray',
  failed: 'red',
}

/// A task's status and whether a session is live are two different things, and the orchestrator's
/// word for the first is "running" either way: a task whose agent finished its turn keeps
/// `status = running` (it is in progress and resumable) with no session behind it. Showing that
/// as "running" says something untrue, so an in-progress task with nothing live reads as **idle**
/// — the work is open, nothing is happening this second.
export function StatusBadge({ status, running }: { status: string; running?: boolean }) {
  const resting = !running && (status === 'running' || status === 'planning')
  return (
    <Flex gap="1" align="center" asChild>
      <span>
        <Badge
          color={resting ? 'gray' : (TASK_COLORS[status] ?? 'gray')}
          variant="soft"
          title={
            resting
              ? `${status}, but no session is live: the work is open and a message resumes it`
              : undefined
          }
        >
          {resting ? 'idle' : status}
        </Badge>
        {running && (
          <Badge color="green" variant="solid" title="a session is running right now">
            live
          </Badge>
        )}
      </span>
    </Flex>
  )
}

/// Time as a person reads it ("4 min ago"), with the exact timestamp on hover — the absolute
/// one still matters when something went wrong.
export function When({ at }: { at: string | null | undefined }) {
  if (!at) return <Text color="gray">—</Text>
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
    <Text color="gray" size="2" title={d.toLocaleString()}>
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
    <Text size="2" color={to ? 'gray' : 'green'}>
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
  color,
  variant,
  confirm,
  onDone,
  disabled,
}: {
  op: string
  path: string
  body?: unknown
  children: React.ReactNode
  color?: 'red' | 'green' | 'gray'
  variant?: 'solid' | 'soft' | 'outline' | 'ghost'
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
    <Flex direction="column" gap="1">
      <Button
        onClick={() => void run()}
        color={color}
        variant={variant ?? 'soft'}
        disabled={busy || disabled}
        loading={busy}
      >
        {children}
      </Button>
      {error && (
        <Text size="1" color="red">
          {error}
        </Text>
      )}
    </Flex>
  )
}

/// A section with a heading, which is most of every page.
export function Section({
  title,
  aside,
  children,
}: {
  title: string
  aside?: React.ReactNode
  children: React.ReactNode
}) {
  return (
    <Box mt="5">
      <Flex justify="between" align="center" mb="2" gap="3" wrap="wrap">
        <Heading size="3">{title}</Heading>
        {aside}
      </Flex>
      {children}
    </Box>
  )
}

/// Agent output and ticket text are untrusted: they are rendered as text, inside a scroll box,
/// never as markup.
export function Pre({ children }: { children: string }) {
  return (
    <Box
      style={{
        maxHeight: '22rem',
        overflow: 'auto',
        background: 'var(--gray-a2)',
        borderRadius: 'var(--radius-3)',
        padding: 'var(--space-3)',
      }}
    >
      <Text as="div" size="1" style={{ whiteSpace: 'pre-wrap', fontFamily: 'var(--code-font-family)' }}>
        {children}
      </Text>
    </Box>
  )
}
