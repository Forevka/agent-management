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

/** Something a task produced: a titled link, or a file the orchestrator stores and serves. */
export type Attachment = {
  id: number
  /** `A-12`, as agents and the brain name it. */
  ref: string
  kind: 'url' | 'file'
  title: string
  url: string | null
  file_name: string | null
  mime: string | null
  size: number | null
  sha256: string | null
  added_by: string
  created_at: string
  /** Where a file downloads from (always as a download, never rendered). */
  download: string | null
}

export type Task = {
  /** The fleet's own short handle (`F-41`), which the owner names this task by. */
  key: string
  /** What this task is about outside the fleet: tickets, merge requests, pages. */
  links?: TaskLink[]
  /** What it produced: links and files, with titles. */
  attachments?: Attachment[]
  /** The agent's closing summary, once it finished. */
  summary?: string | null
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
  /** When the nightly archive put this done task off the board; it is still `done`. */
  archived_at?: string | null
  sessions?: number
  brief?: string
  model?: string | null
  effort?: string | null
  run_id?: string | null
  topic_id?: number | null
  /** What its runs have cost so far, all together; `null` before the first turn ended. */
  cost?: TaskCost | null
}

/** What a task's runs cost: dollars at API prices, every kind of token, turns. */
export type TaskCost = { usd: number; tokens: number; turns: number }

/** A node's Claude usage, from Claude's last word on it. Shares are 0..1. */
export type ClaudeUsage = {
  at: string | null
  status: string | null
  five_hour: number | null
  seven_day: number | null
  /** Until when it is out of usage: new task runs there wait. */
  limited_until: string | null
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
  /** What it cost at API prices, once a turn ended. */
  cost_usd?: number | null
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
  /** Free space where its sessions write; `null` before the node's first report. */
  disk: NodeDisk | null
  claude_usage?: ClaudeUsage
  /** Where it clones assigned projects; `null` for its default, `~/fleet-projects`. */
  projects_root: string | null
  known_projects?: { slug: string; name: string; repo_url: string | null }[]
  /** The projects the owner assigned to it, and how each stands there. */
  assignments?: NodeAssignment[]
  /** Whether it reads assignments: an older node clones nothing until it has updated. */
  reads_assignments?: boolean
  sessions?: { run_id: string; task: string; started_at: string; status: string }[]
}

/**
 * How an assigned project stands on its node: mapped (`path`), being cloned, failed (`detail`
 * says why), waiting for a task to need it, without a repository URL, or on a node that must
 * update before it clones anything.
 */
export type AssignmentStatus = 'mapped' | 'cloning' | 'failed' | 'waiting' | 'no_url' | 'node_behind'

export type NodeAssignment = {
  project: string
  name: string
  node_id: string
  node: string
  online: boolean
  repo_url: string | null
  status: AssignmentStatus
  path: string | null
  state: string | null
  detail: string | null
  assigned_at: string
}

/** `low` warned the owner; `full` holds new task runs on the node until space is freed. */
export type DiskState = 'ok' | 'low' | 'full'

export type NodeDisk = {
  state: DiskState
  at: string | null
  claude_dir: string
  /** Bytes free on the fullest volume the fleet writes to. */
  least_free: number | null
  volumes: { volume: string; free: number; total: number; claude_dir: boolean; projects: string[] }[]
  biggest: { name: string; bytes: number }[]
}

/** `3.2 GB`, `512 MB`, as the orchestrator writes sizes. */
export function formatBytes(n: number): string {
  const gb = 1024 ** 3
  return n >= gb ? `${(n / gb).toFixed(1)} GB` : `${Math.round(n / 1024 ** 2)} MB`
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
  /** Its agent is asleep until a wakeup: the next timer, if any, and whether it also waits on a condition. */
  wakeup: { at: string | null; condition: boolean } | null
}

export type Overview = {
  items: Item[]
  tasks: Task[]
  nodes: Node[]
  /** `color` is one of the project palette's names (`--fx-hue-<color>`). */
  projects: { slug: string; name: string; open: number; running: number; total: number; color: string }[]
  todos_open: number
  sessions_today: number
  /** This orchestrator's release (`v0.2.17`; `0.1.0-dev` for a hand build). */
  release?: string
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
  /// Where its code lives, for the nodes it is assigned to that clone it.
  repo_url?: string | null
  /// Its colour (a palette name), and whether the owner chose it or it is the slug's default.
  color?: string
  color_set?: boolean
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
  color: string
}

export type ProjectDetail = {
  project: Project
  folders: { id: number; name: string; bound: boolean }[]
  channels: { source: string; channel_id: string; name: string; triage: boolean }[]
  nodes: { id: string; name: string; online: boolean; path: string | null }[]
  /** The nodes it is assigned to (cloned there on demand), and how it stands on each. */
  assigned?: NodeAssignment[]
  tasks: Task[]
  /** Every open task (not done, stopped or failed), however old; `tasks` is only the latest. */
  open?: Task[]
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
/// One ticket of a roadmap sprint. `title` and `jira_status` are Jira's text: shown, never acted on.
export type RoadmapItem = {
  id: number
  key: string
  project: string
  title: string
  url: string | null
  jira_status: string | null
  jira_done: boolean
  type: string | null
  priority: string | null
  /** Percent of one usage window. */
  usage: number
  usage_note: string | null
  estimated_by: string | null
  /** new, approved, declined, skipped, started, left */
  state: string
  approved: boolean
  pinned: boolean
  /** The window it is settled in (placed by hand, or where it started). */
  at: { day: number; session: number } | null
  /** Where the plan puts it, piece by piece; empty when there is no room this sprint. */
  planned: { day: number; session: number; usage: number }[]
  task: { id: string; key: string; status: string; title: string } | null
  /** Whether the server's plan takes it, and how much of it: the board must agree with both. */
  on_board: boolean
  plan_usage: number
  /** Why its last start failed, while it keeps failing. */
  start_error: string | null
}

export type SprintRef = { id: number; name: string; starts_on: string; ends_on: string; span: string; current: boolean }

export type Roadmap = {
  enabled: boolean
  sprint: SprintRef
  sprints: SprintRef[]
  days: { name: string; date: string }[]
  sessions: string[]
  window_minutes: number
  limit: number
  now: { day: number; session: number; open: boolean }
  items: RoadmapItem[]
  open_card: number | null
  sync: {
    at: string
    sprint: number
    projects: Record<string, { ok: boolean; tickets?: number; error?: string }>
  } | null
}

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
    /// One line on where it stands ("fired …; no report from the brain yet"), the same line
    /// `/habits` shows in Telegram.
    status: string
    last_checked_at: string | null
    last_fired_at: string | null
    last_report_at: string | null
    /// Newest first: what it did, what it looked at and left, and what the brain reported.
    log: {
      at: string
      kind: 'fired' | 'skipped' | 'failed' | 'report'
      source: 'orchestrator' | 'brain'
      task: string | null
      candidate: string | null
      what: string
    }[]
  }[]
}

/// One of the owner's colleagues, as the people records know them. Written by the daily
/// retrospective from what it read (untrusted text: rendered as text, never markup), and edited
/// by the owner, which makes `updated_by` "owner".
export type Person = {
  id: number
  name: string
  position: string | null
  brief: string | null
  /** The company's projects they work on: free text, not necessarily fleet projects. */
  projects: string[]
  /** `en`, `uk`, or not known. */
  language: 'en' | 'uk' | null
  observations: string[]
  tags: string[]
  last_interaction: string | null
  created_at: string
  updated_at: string
  updated_by: 'retro' | 'owner'
  /** The fields the owner set themselves: the retrospective may not change them (it may still
   * add to observations, tags and contacts). */
  owner_fields: string[]
  /** When the owner last edited the record; null if they never have. */
  owner_edited_at: string | null
  /** What the owner removed, which the retrospective may not put back (`obs:…`, `tag:…`,
   * `contact:<source>|<user id or handle>`). */
  owner_removed: string[]
  contacts: { id: number; source: string; handle: string; native_id: string | null }[]
}

export type People = {
  people: Person[]
  /** Every record, whatever the filters. */
  total: number
  /** Tags and projects in use, most frequent first: the recurring tags are the ones worth a field. */
  tags: { tag: string; count: number }[]
  projects: { project: string; count: number }[]
  sources: string[]
}

export type PersonDetail = {
  person: Person
  /** What was done to the record lately, and by whom (`retro` or `owner`). */
  history: { at: string; actor: string; action: string; changed: string[] | null }[]
  sources: string[]
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


// ---------------------------------------------------------------- the kit

export type KitKind = 'skill' | 'plugin' | 'mcp'

/** `pending` until the node reports; `removed` once it took the kit's own copy away. */
export type KitState = 'pending' | 'installed' | 'failed' | 'conflict' | 'removed'

export type KitItem = {
  id: number
  kind: KitKind
  /** A skill's directory, `plugin@marketplace`, or the MCP server's name. */
  name: string
  enabled: boolean
  /** A plugin's `{plugin, marketplace, marketplace_source}`; an MCP server's summary only,
   *  `{type, command?, endpoint?}`: its config is sealed in the vault and never sent here. */
  spec: Record<string, unknown>
  /** A skill's files, path and size; the item endpoint has their text. */
  files: { path: string; bytes: number }[] | null
  updated_at: string
}

export type Kit = {
  items: KitItem[]
  nodes: {
    id: string
    name: string
    online: boolean
    version: string
    current: boolean
    /** Why the node cannot hold agent sessions to the kit's MCP servers (an old claude CLI). */
    note: string | null
  }[]
  status: { node: string; item: number; state: KitState; detail: string; at: string }[]
}

export type KitItemDetail = Omit<KitItem, 'files'> & {
  files: Record<string, string> | null
  /** An MCP server's config, masked: every secret it could find reads `[masked]`. */
  config: Record<string, unknown> | null
}

/** What a node reported having, to choose from. Secrets show as the placeholders they would get. */
export type KitCapture = {
  capture: string
  node: string
  skills: { name: string; in_kit: boolean; files: { path: string; bytes: number }[] }[]
  plugins: { name: string; in_kit: boolean; marketplace_source: string }[]
  mcp: {
    name: string
    in_kit: boolean
    summary: Record<string, unknown>
    /** The config masked; the real one is sealed in the vault on import. */
    config: Record<string, unknown>
    /** How many values were masked. */
    masked: number
  }[]
  skipped: { kind: string; name: string; reason: string }[]
}