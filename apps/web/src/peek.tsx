import { createContext, useContext, type MouseEvent, type ReactNode } from 'react'
import { Link, useLocation, useNavigate, useSearchParams, type To } from 'react-router-dom'

/*
 * A task opened beside the page, in the shell's work area, rather than instead of it.
 *
 * It lives in the URL as `?task=<id>` on whatever page is open, like every other filter here: a
 * link to the board with a task open is a link to exactly that, back and forward step through what
 * was opened, and the full page (`/tasks/<id>`) is still one click away inside it.
 */

export const PEEK = 'task'

/** Brings the work area forward again: the shell provides it, for the task that is already open. */
export const RevealTask = createContext<(() => void) | null>(null)

export function useTaskPeek() {
  const [params] = useSearchParams()
  const { pathname } = useLocation()
  const navigate = useNavigate()
  const reveal = useContext(RevealTask)
  const id = params.get(PEEK)
  /** Where a click on a task goes: this page, with the task open beside it. */
  const to = (task: string): To => {
    const next = new URLSearchParams(params)
    next.set(PEEK, task)
    return { pathname, search: `?${next}` }
  }
  /** As a link does: a new history entry, so back returns to what was open before. The task
   *  that is already open is only brought forward (its panel may have been folded away). */
  const open = (task: string) => {
    if (task === id) reveal?.()
    else navigate(to(task))
  }
  /** For a link to a task: a plain click on the open one reveals it rather than navigating. */
  const onClick = (task: string) => (e: MouseEvent) => {
    if (task !== id || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return
    e.preventDefault()
    reveal?.()
  }
  const close = () => {
    const next = new URLSearchParams(params)
    next.delete(PEEK)
    const search = next.toString()
    navigate({ pathname, search: search ? `?${search}` : '' }, { replace: true })
  }
  return { id, to, open, close, onClick }
}

/** A link that opens a task beside the page. A new tab (Ctrl+click) gets the same page with it open. */
export function TaskLink({ id, className = 'fx-link', children }: { id: string; className?: string; children: ReactNode }) {
  const { to, onClick } = useTaskPeek()
  return (
    <Link to={to(id)} className={className} onClick={onClick(id)}>
      {children}
    </Link>
  )
}
