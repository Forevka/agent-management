import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardFooter, CardHeader } from '@/components/ui/card'
import { TextArea } from '@/components/ui/text-area'
import { TextField } from '@/components/ui/text-field'
import { Page, PageHeader, Row, Section, Stack, Text } from '@/components/fx/layout'
import { Note } from '@/components/fx/note'
import { ArrowLeftIcon } from '@/components/fx/icons'
import * as api from '../api'
import type { KitItemDetail } from '../api'
import { ActionButton, ErrorNote, Loaded, useApi } from '../ui'

/// One skill's files, or one MCP server's config, to edit. Saving sends the whole item; the
/// nodes get it at once.
export default function KitItem() {
  const { id = '' } = useParams()
  const state = useApi<KitItemDetail>(`/api/kit/${encodeURIComponent(id)}`)
  const navigate = useNavigate()

  return (
    <Page>
      <Loaded state={state}>
        {(d) => (
          <>
            <PageHeader
              title={d.name}
              back={
                <Link to="/kit" className="fx-link" data-muted="">
                  <ArrowLeftIcon /> The kit
                </Link>
              }
              meta={
                <Row gap="2">
                  <Badge>{d.kind}</Badge>
                  <Badge tone={d.enabled ? 'success' : 'neutral'} dot>
                    {d.enabled ? 'on every node' : 'off'}
                  </Badge>
                </Row>
              }
            />
            {d.kind === 'skill' && <SkillEditor item={d} onSaved={() => void state.reload()} />}
            {d.kind === 'mcp' && <McpEditor item={d} onSaved={() => void state.reload()} />}
            <Section title="Danger">
              <Card>
                <CardContent>
                  <Row gap="3" justify="between">
                    <Text size="2" tone="muted">
                      Deleting it takes it off every node where the kit installed it. A machine’s own item of the same
                      name is never touched.
                    </Text>
                    <ActionButton
                      path="/api/kit"
                      op="delete"
                      body={{ id: d.id }}
                      variant="destructive"
                      confirm={`Delete ${d.name} from the kit?`}
                      onDone={() => navigate('/kit')}
                    >
                      Delete
                    </ActionButton>
                  </Row>
                </CardContent>
              </Card>
            </Section>
          </>
        )}
      </Loaded>
    </Page>
  )
}

function useSave(onSaved: () => void) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [done, setDone] = useState<string | null>(null)
  const save = async (op: string, body: unknown) => {
    setBusy(true)
    setError(null)
    setDone(null)
    try {
      const r = await api.act<{ done: string }>('/api/kit', op, body)
      setDone(r.done)
      onSaved()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }
  return { busy, error, done, save }
}

function SkillEditor({ item, onSaved }: { item: KitItemDetail; onSaved: () => void }) {
  const [files, setFiles] = useState<Record<string, string>>(item.files ?? {})
  const [newPath, setNewPath] = useState('')
  const { busy, error, done, save } = useSave(onSaved)
  useEffect(() => setFiles(item.files ?? {}), [item])
  // SKILL.md first, the rest by path.
  const paths = Object.keys(files).sort((a, b) => (a === 'SKILL.md' ? -1 : b === 'SKILL.md' ? 1 : a.localeCompare(b)))

  return (
    <Section title="Files" description="Text files, relative to the skill’s directory. SKILL.md is required.">
      <Card>
        <CardContent>
          <Stack gap="4">
            {paths.map((p) => (
              <Stack key={p} gap="1">
                <TextArea
                  label={
                    <Text mono size="2">
                      {p}
                    </Text>
                  }
                  value={files[p]}
                  minRows={p === 'SKILL.md' ? 8 : 3}
                  maxRows={30}
                  expandable
                  onChange={(e) => {
                    const text = e.currentTarget.value
                    setFiles((f) => ({ ...f, [p]: text }))
                  }}
                />
                {p !== 'SKILL.md' && (
                  <Row>
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() =>
                        setFiles((f) => {
                          const next = { ...f }
                          delete next[p]
                          return next
                        })
                      }
                    >
                      Remove {p}
                    </Button>
                  </Row>
                )}
              </Stack>
            ))}
            <Row gap="2" align="end">
              <div style={{ minWidth: '16rem' }}>
                <TextField
                  label="Another file"
                  size="sm"
                  placeholder="reference/notes.md"
                  value={newPath}
                  onChange={(e) => setNewPath(e.currentTarget.value)}
                />
              </div>
              <Button
                size="sm"
                variant="secondary"
                disabled={!newPath.trim() || newPath.trim() in files}
                onClick={() => {
                  setFiles((f) => ({ ...f, [newPath.trim()]: '' }))
                  setNewPath('')
                }}
              >
                Add file
              </Button>
            </Row>
            {error && <ErrorNote>{error}</ErrorNote>}
            {done && (
              <Note tone="success" title="Saved">
                {done}
              </Note>
            )}
          </Stack>
        </CardContent>
        <CardFooter>
          <Button size="sm" loading={busy} onClick={() => void save('skill_save', { name: item.name, files })}>
            Save
          </Button>
        </CardFooter>
      </Card>
    </Section>
  )
}

function McpEditor({ item, onSaved }: { item: KitItemDetail; onSaved: () => void }) {
  const [text, setText] = useState(JSON.stringify(item.spec, null, 2))
  const { busy, error, done, save } = useSave(onSaved)
  useEffect(() => setText(JSON.stringify(item.spec, null, 2)), [item])
  return (
    <Section
      title="Config"
      description="As Claude Code takes it: command, args and env, or type, url and headers. A ${secret:NAME} placeholder stands for a value in the vault; a new value typed here moves into the vault when you save."
    >
      <Card>
        <CardHeader title="Secrets" description="Set or replace a value on the kit page, under Secrets. Values are never shown." />
        <CardContent>
          <Stack gap="3">
            <TextArea
              aria-label="MCP server config"
              value={text}
              minRows={8}
              maxRows={30}
              style={{ fontFamily: 'var(--aui-font-mono, monospace)' }}
              onChange={(e) => setText(e.currentTarget.value)}
            />
            {error && <ErrorNote>{error}</ErrorNote>}
            {done && (
              <Note tone="success" title="Saved">
                {done}
              </Note>
            )}
          </Stack>
        </CardContent>
        <CardFooter>
          <Button size="sm" loading={busy} onClick={() => void save('mcp_save', { name: item.name, config: text })}>
            Save
          </Button>
        </CardFooter>
      </Card>
    </Section>
  )
}
