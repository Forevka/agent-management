// The one place that talks to the orchestrator. Everything here mirrors
// crates/orchestrator/src/api.rs; if a shape changes there, it changes here.

/// A 401 is not an error to show in a toast: it means the session is gone and the whole app
/// should offer to sign in again. It is thrown as its own type so callers can tell.
export class Unauthorized extends Error {}

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message)
  }
}

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    // Same-origin, so the session cookie rides along; `SameSite=Strict` is what keeps anyone
    // else from doing this.
    credentials: 'same-origin',
    headers: init?.body ? { 'content-type': 'application/json' } : {},
  })
  if (res.status === 401) throw new Unauthorized('sign in again')
  const text = await res.text()
  const body = text ? JSON.parse(text) : {}
  if (!res.ok) throw new ApiError(body.error ?? `request failed (${res.status})`, res.status)
  return body as T
}

export const get = <T,>(path: string) => call<T>(path)

/// Every mutation is `{op, body}`, as the server expects.
export const act = <T = { done: unknown },>(path: string, op: string, body: unknown = {}) =>
  call<T>(path, { method: 'POST', body: JSON.stringify({ op, body }) })

// ---------------------------------------------------------------- shapes

/// The kinds that hold a tool call open, where the agent's process really is stopped until the
/// owner answers. An item's `blocking` is not that: it means "the owner is needed", and an `ask`
/// or a proposal blocks nobody.
export const HOLDS_THE_AGENT = ['plan', 'question', 'permission']

export type Item = {
  id: number
  kind: string
  blocking: boolean
  project: string | null
  task: string | null
  agent: string | null
  created_at: string
  summary: string
  state?: string
  /// What the owner may see of the call behind the item, secrets redacted: the question, the
  /// questions, the plan or the proposed change, depending on the kind.
  payload?: Record<string, unknown>
  task_title?: string
  related?: Item[]
}

/** One thing a task belongs to in somebody else's tracker. */
export type TaskLink = {
  id: number
  /** `jira`, `mr`, `pr`, `url`, or something newer the UI shows plainly. */
  kind: string
  /** How a person says it: `BIOINS-5068`, `!940`, `#482`, or the URL. */
  ref: string
  /** Where it is, when the fleet could work that out. */
  url: string | null
  added_by: string
}

export type Task = {
  /** The fleet's own short handle (`F-41`), which the owner names this task by. */
  key: string
  /** What this task is about outside the fleet: tickets, merge requests, pages. */
  links?: TaskLink[]
  id: string
  project: string
  title: string
  status: string
  running: boolean
  agent: string | null
  node: string | null
  session: string | null
  created_at: string
  updated_at: string
  sessions?: number
  brief?: string
  model?: string | null
  effort?: string | null
  run_id?: string | null
  topic_id?: number | null
}

/// A session is one agent process the fleet started. The fleet-wide list carries the task and
/// project it belonged to; a task's own list leaves those out, because they are the page.
export type Session = {
  run_id: string
  task?: string
  project?: string
  agent?: string | null
  node: string | null
  session: string | null
  status: string
  outcome: string | null
  started_at: string
  ended_at: string | null
  title?: string | null
}

export type Node = {
  id: string
  name: string
  os: string
  version: string
  online: boolean
  last_seen: string | null
  command: string[]
  auth_mode: string
  has_auth_secret: boolean
  max_agents: number
  running: string[]
  projects: { project: string; path: string }[]
  known_projects?: { slug: string; name: string }[]
  sessions?: { run_id: string; task: string; started_at: string; status: string }[]
}

/** One card on the board: a task, plus what the card shows, so the page runs no query itself. */
export type BoardTask = Task & {
  brief: string
  sessions: number
  milestones: number
  milestones_done: number
  /** When the state the card shows began, for the duration it counts up. */
  since: string
  /** What this task wants from the owner, if anything. The blocking item wins. */
  needs_you: { id: number; kind: string; blocking: boolean; since: string } | null
}

export type Overview = {
  items: Item[]
  tasks: Task[]
  nodes: Node[]
  projects: { slug: string; name: string; open: number; running: number; total: number }[]
  todos_open: number
  sessions_today: number
}

/// A project as the detail endpoint describes it, where `jira` is the full mapping.
export type Project = {
  slug: string
  name: string
  description: string
  folders?: string[]
  jira?: { key: string | null; board: number | null; credential_folder: string | null }
  /// Who approves plans of its tasks that are not workflows (a workflow has its own, on its card).
  plan_gate?: 'owner' | 'auto'
}

/// A row of the project list, which is the brain's `admin_read` shape: here `jira` is just the
/// project key, or null. Deliberately a separate type — the two endpoints really do differ.
export type ProjectRow = {
  slug: string
  name: string
  description: string
  nodes: { node: string; online: boolean }[]
  open_tasks: number
  jira: string | null
}

export type ProjectDetail = {
  project: Project
  folders: { id: number; name: string; bound: boolean }[]
  channels: { source: string; channel_id: string; name: string; triage: boolean }[]
  nodes: { id: string; name: string; online: boolean; path: string | null }[]
  tasks: Task[]
  sessions: number
  open_tasks: number
}

export type Todo = {
  id: number
  title: string
  detail: string
  refs: string[]
  status: string
  created_by: string
  created_at: string
  updated_at: string
  due: string | null
  workflow: string | null
}

export type Inbox = {
  todos: Todo[]
  rules: { id: number; text: string; active: boolean; source: string; cited: string | null; created_at: string }[]
  triage: { at: string; refs: string[]; decision: string; note: string }[]
  stats: { pending: number; batches_today: number; messages_today: number }
}

/// The daily retrospective: what the fleet concluded about its own days, and what it proposes
/// doing about it. `detail` is a session's own prose — rendered as text, never as markup.
export type Retro = {
  proposals: {
    id: number
    area: string
    title: string
    detail: string
    state: string
    task: string | null
    run: number | null
  }[]
  runs: { id: number; day: string; verdict: string | null; proposals: number }[]
}

/// A habit: something the owner kept doing that the fleet now does for them. `what` is the
/// server's own one-line rendering of it, so the page and Telegram say the same thing.
export type Habits = {
  habits: {
    id: number
    kind: string
    what: string
    project: string | null
    schedule: string | null
    brief: string | null
    why: string
    active: boolean
    paused_why: string | null
    cap: number
    created_at: string
    last_used_at: string | null
    uses: { at: string; what: string }[]
  }[]
}

export type Agents = {
  agents: {
    id: string
    name: string
    role: string
    project: string | null
    bot: number | null
    preferred_node: string | null
    model: string | null
    effort: string | null
  }[]
  bots: { id: number; username: string; agent: string | null }[]
  efforts: string[]
  default_model: string
  suggested_models: string[]
}

export type Secrets = {
  folders: { id: number; name: string; preset: string | null; keys: number; projects: string[] }[]
  presets: { name: string; keys: string[] }[]
}

export type SecretFolder = {
  id: number
  name: string
  preset: string | null
  expected: string[]
  keys: { key: string }[]
  projects: string[]
}

export type TaskDetail = {
  task: Task
  milestones: { idx: number; title: string; done: boolean }[]
  sessions: Session[]
  items: Item[]
}

export type Events = {
  events: { id: number; at: string; kind: string; data: string }[]
  more: boolean
}
