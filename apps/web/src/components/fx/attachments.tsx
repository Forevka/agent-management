/*
 * A task's attachments: what it produced, as links and files.
 *
 * Ours (`fx-`), to the same contract as the rest: one file, styles beside it, colour from tokens.
 * The list is plain anchors — a link opens where it points, a file downloads from the
 * orchestrator (which serves it as a download whatever it is, so nothing an agent made can run
 * here). The form adds a link, or uploads a file in 1 MiB pieces through the same
 * begin/chunk/finish the agents use, so the limits and the secret check are the server's alone.
 */

import { useRef, useState } from 'react'
import { Button } from '@/components/ui/button'
import { TextField } from '@/components/ui/text-field'
import type { Attachment } from '../../api'
import * as api from '../../api'
import { ExternalIcon, FileIcon, PaperclipIcon } from './icons'

export function size(n: number | null | undefined): string {
  const b = n ?? 0
  if (b < 1024) return `${b} B`
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`
  return `${(b / 1024 / 1024).toFixed(1)} MB`
}

function host(url: string): string {
  try {
    return new URL(url).host
  } catch {
    return url
  }
}

export function AttachmentList({
  items,
  onRemove,
}: {
  items: Attachment[]
  onRemove?: (a: Attachment) => void
}) {
  return (
    <ul className="fx-attach">
      <style href="fx-attach" precedence="default">
        {css}
      </style>
      {items.map((a) => {
        const href = a.kind === 'file' ? (a.download ?? '#') : (a.url ?? '#')
        return (
          <li key={a.id} className="fx-attach__item">
            <span className="fx-attach__icon" aria-hidden="true">
              {a.kind === 'file' ? <FileIcon /> : <ExternalIcon />}
            </span>
            <span className="fx-attach__body">
              <a
                className="fx-attach__title"
                href={href}
                {...(a.kind === 'url' ? { target: '_blank', rel: 'noreferrer noopener' } : { download: a.file_name ?? '' })}
              >
                {a.title}
              </a>
              <span className="fx-attach__meta">
                {a.ref} · {a.kind === 'file' ? `${a.file_name} · ${size(a.size)}` : host(a.url ?? '')} · by {a.added_by}
              </span>
            </span>
            {onRemove && (
              <Button size="sm" variant="ghost" onClick={() => onRemove(a)} aria-label={`Remove ${a.title}`}>
                Remove
              </Button>
            )}
          </li>
        )
      })}
    </ul>
  )
}

async function sha256(file: File): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', await file.arrayBuffer())
  return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, '0')).join('')
}

function base64(bytes: Uint8Array): string {
  let s = ''
  // In slices: spreading a whole megabyte into one call overflows the argument limit.
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode(...bytes.subarray(i, i + 0x8000))
  }
  return btoa(s)
}

/// Adds a link or a file to the task at `path` (`/api/tasks/<id>/attachments`).
export function AttachForm({ path, onDone }: { path: string; onDone: () => void }) {
  const [title, setTitle] = useState('')
  const [url, setUrl] = useState('')
  const [file, setFile] = useState<File | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const input = useRef<HTMLInputElement>(null)

  const reset = () => {
    setTitle('')
    setUrl('')
    setFile(null)
    if (input.current) input.current.value = ''
  }

  const submit = async () => {
    setError(null)
    try {
      if (file) {
        setBusy('Hashing…')
        const begin = await api.act<{ upload: number; chunk_size: number }>(path, 'begin', {
          title: title || file.name,
          file_name: file.name,
          size: file.size,
          sha256: await sha256(file),
        })
        const n = Math.ceil(file.size / begin.chunk_size)
        for (let i = 0; i < n; i++) {
          setBusy(`Uploading ${i + 1}/${n}…`)
          const piece = new Uint8Array(
            await file.slice(i * begin.chunk_size, (i + 1) * begin.chunk_size).arrayBuffer(),
          )
          await api.act(path, 'chunk', { upload: begin.upload, index: i, data: base64(piece) })
        }
        setBusy('Checking…')
        await api.act(path, 'finish', { upload: begin.upload })
      } else {
        setBusy('Attaching…')
        await api.act(path, 'url', { url, title })
      }
      reset()
      onDone()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  const ready = file ? true : url.trim() !== '' && title.trim() !== ''
  return (
    <div className="fx-attach__form">
      <style href="fx-attach" precedence="default">
        {css}
      </style>
      <div className="fx-attach__fields">
        <TextField
          size="sm"
          label="Title"
          placeholder={file ? file.name : 'What it is, as you would look for it'}
          value={title}
          onChange={(e) => setTitle(e.currentTarget.value)}
        />
        <TextField
          size="sm"
          label="Link"
          placeholder="https://…"
          value={url}
          disabled={file !== null}
          onChange={(e) => setUrl(e.currentTarget.value)}
        />
      </div>
      <div className="fx-attach__actions">
        <label className="fx-attach__pick">
          <PaperclipIcon />
          <span>{file ? `${file.name} (${size(file.size)})` : 'or choose a file'}</span>
          <input
            ref={input}
            type="file"
            onChange={(e) => {
              const f = e.currentTarget.files?.[0] ?? null
              setFile(f)
              if (f) setUrl('')
            }}
          />
        </label>
        {file && (
          <Button size="sm" variant="ghost" onClick={reset}>
            Clear
          </Button>
        )}
        <Button size="sm" onClick={() => void submit()} disabled={!ready || busy !== null} loading={busy !== null}>
          {busy ?? 'Attach'}
        </Button>
      </div>
      {error && <p className="fx-attach__error">{error}</p>}
    </div>
  )
}

const css = /* css */ `
.fx-attach {
  --fx-attach-ink: var(--aui-ink, #171717);
  --fx-attach-muted: var(--aui-muted, #737373);
  --fx-attach-border: var(--aui-border, #e5e5e5);
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
}
.fx-attach__item {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 0;
  border-bottom: 1px solid var(--fx-attach-border);
  min-width: 0;
}
.fx-attach__item:last-child {
  border-bottom: 0;
}
.fx-attach__icon {
  flex: none;
  display: grid;
  place-items: center;
  width: 28px;
  height: 28px;
  border-radius: 8px;
  border: 1px solid var(--fx-attach-border);
  color: var(--fx-attach-muted);
  font-size: 15px;
}
.fx-attach__body {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  flex: 1;
}
.fx-attach__title {
  color: var(--fx-attach-ink);
  font-weight: 500;
  font-size: 14px;
  text-decoration: none;
  overflow-wrap: anywhere;
}
.fx-attach__title:hover {
  text-decoration: underline;
}
.fx-attach__meta {
  color: var(--fx-attach-muted);
  font-size: 12px;
  overflow-wrap: anywhere;
}
.fx-attach__form {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.fx-attach__fields {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
  gap: 10px;
}
.fx-attach__actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.fx-attach__pick {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  flex: 1;
  min-width: 0;
  color: var(--aui-muted, #737373);
  font-size: 13px;
  cursor: pointer;
}
.fx-attach__pick input {
  position: absolute;
  width: 1px;
  height: 1px;
  opacity: 0;
}
.fx-attach__pick:focus-within span {
  text-decoration: underline;
}
.fx-attach__error {
  margin: 0;
  color: var(--aui-danger, #c62828);
  font-size: 13px;
}
:where(.dark, [data-theme="dark"]) .fx-attach {
  --fx-attach-ink: var(--aui-ink, #fafafa);
  --fx-attach-muted: var(--aui-muted, #a3a3a3);
  --fx-attach-border: var(--aui-border, #2a2a2a);
}
`
