import { useEffect } from 'react'
import { Link } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Facts } from '@/components/fx/table'
import { Row, Stack, Text } from '@/components/fx/layout'
import { AttachmentList } from '@/components/fx/attachments'
import { CheckIcon } from '@/components/fx/icons'
import type { TaskDetail as Detail } from '../api'
import { ActionButton, Loaded, Pre, StatusBadge, When, useApi } from '../ui'
import { BUILTIN, FINISHED, ItemCard, MessageBox, isOpen } from './TaskDetail'

/*
 * A task in the work area: what the owner needs to see without leaving the page they are on —
 * where it stands, what it waits on them for, its checklist, what it produced — and the two things
 * they most often do from there, writing to its agent and stopping it. Everything else (editing,
 * links, the session list, the transcript) is on the full page, one click away.
 */
export function TaskPeek({ id, onKey }: { id: string; /** Its F-key once known, for the tab */ onKey?: (key: string) => void }) {
  const path = `/api/tasks/${encodeURIComponent(id)}`
  const state = useApi<Detail>(path, 10_000)
  const enc = encodeURIComponent(id)
  const key = state.data?.task.key
  useEffect(() => {
    if (key) onKey?.(key)
    // Only a new key matters; the callback is a fresh function every render of the shell.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key])
  return (
    <div className="fx-peek">
      <style href="fx-peek" precedence="default">
        {css}
      </style>
      <Loaded state={state}>
        {(d) => {
          const t = d.task
          const builtin = BUILTIN.includes(t.id)
          const open = d.items.filter(isOpen)
          const done = d.milestones.filter((m) => m.done).length
          return (
            <Stack gap="4">
              <Stack gap="2">
                <Row gap="2">
                  <Badge>{t.key || t.id}</Badge>
                  <StatusBadge status={t.status === 'done' && t.archived_at ? 'archived' : t.status} running={t.running} />
                </Row>
                <Text size="3" weight="medium" breakAll>
                  {t.title || t.id}
                </Text>
                <Row gap="3">
                  <Link to={`/tasks/${enc}`} className="fx-link">
                    Open task
                  </Link>
                  <Link to={`/tasks/${enc}/session`} className="fx-link">
                    {t.running ? 'Watch live' : 'Open session'}
                  </Link>
                  {!builtin && (t.running || !FINISHED.includes(t.status)) && (
                    <ActionButton
                      path={path}
                      op="stop"
                      variant="ghost"
                      confirm={`Stop ${t.key}? Its agent is stopped now; a message resumes the task later.`}
                      onDone={() => void state.reload()}
                    >
                      Stop
                    </ActionButton>
                  )}
                </Row>
              </Stack>

              <Facts
                items={[
                  [
                    'Project',
                    <Link to={`/projects/${encodeURIComponent(t.project)}`} className="fx-link">
                      {t.project}
                    </Link>,
                  ],
                  ['Agent', t.agent ?? '—'],
                  ['Node', t.node ?? '—'],
                  ['Created', <When at={t.created_at} />],
                ]}
              />

              {open.length > 0 && (
                <Stack gap="2">
                  <Text size="1" weight="medium" tone="muted">
                    Waiting on you
                  </Text>
                  {open.map((i) => (
                    <ItemCard key={i.id} item={i} />
                  ))}
                </Stack>
              )}

              {d.milestones.length > 0 && (
                <Stack gap="2">
                  <Text size="1" weight="medium" tone="muted">
                    Checklist · {done} of {d.milestones.length}
                  </Text>
                  {d.milestones.map((m) => (
                    <Row key={m.idx} gap="2" align="start" wrap={false}>
                      <span className="fx-check" data-done={m.done || undefined} aria-hidden="true">
                        {m.done ? <CheckIcon /> : null}
                      </span>
                      <Text size="1" tone={m.done ? 'muted' : 'default'} breakAll>
                        {m.title}
                      </Text>
                    </Row>
                  ))}
                </Stack>
              )}

              {(t.attachments ?? []).length > 0 && (
                <Stack gap="2">
                  <Text size="1" weight="medium" tone="muted">
                    Attachments
                  </Text>
                  <AttachmentList items={t.attachments ?? []} />
                </Stack>
              )}

              {/* What it came to, once it did; until then, what it was asked. */}
              <Stack gap="2">
                <Text size="1" weight="medium" tone="muted">
                  {t.summary ? 'Summary' : 'Brief'}
                </Text>
                <Pre>{t.summary || t.brief || ''}</Pre>
              </Stack>

              {!builtin && (
                <Stack gap="2">
                  <Text size="1" weight="medium" tone="muted">
                    Message the agent
                  </Text>
                  <MessageBox task={t} onSent={() => void state.reload()} />
                </Stack>
              )}
            </Stack>
          )
        }}
      </Loaded>
    </div>
  )
}

// The checklist's tick is TaskDetail's `.fx-check`; that page's stylesheet is not loaded here, so
// the panel carries the same rule under the same name (React dedupes by `href`, not by rule).
const css = /* css */ `
.fx-peek {
  padding: 16px 18px 28px;
  min-width: 0;
}
.fx-peek .fx-check {
  display: grid;
  place-items: center;
  flex: none;
  width: 15px;
  height: 15px;
  margin-top: 2px;
  border: 1.5px solid var(--aui-border-strong, #d4d4d8);
  border-radius: 999px;
  color: transparent;
  font-size: 10px;
}
.fx-peek .fx-check[data-done] {
  background: var(--aui-success, #1e8a4f);
  border-color: var(--aui-success, #1e8a4f);
  color: #fff;
}
`
