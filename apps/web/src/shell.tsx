import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { ProjectsProvider } from '@/components/fx/project'
import { Outlet, matchPath, useLocation, useNavigate, useNavigationType } from 'react-router-dom'
import type { IconSvgElement } from '@hugeicons/react'
import {
  AiBrain01Icon,
  Calendar03Icon,
  CheckListIcon,
  ComputerIcon,
  Folder01Icon,
  FolderLibraryIcon,
  InboxIcon,
  KanbanIcon,
  Key01Icon,
  Loading03Icon,
  Moon02Icon,
  PackageIcon,
  PaintBoardIcon,
  PlayCircleIcon,
  RepeatIcon,
  RoboticIcon,
  Settings01Icon,
  Sun03Icon,
  Task01Icon,
  UserGroupIcon,
  HardDriveIcon,
} from '@hugeicons/core-free-icons'
import { AppShell, Icon, useWorkArea, type MenuItem, type ShellGroup, type ShellNavItem } from '@/components/ui/app-shell'
import type { CommandGroup } from '@/components/ui/command-menu-clean'
import { useTheme, type Mode } from '@/components/fx/theme'
import type { Overview } from './api'
import { useApi } from './ui'
import { RevealTask, useTaskPeek } from './peek'
import { TaskPeek } from './pages/TaskPeek'

/*
 * The frame: affectUI's App Shell (Pro, fetched at build time; see apps/web/DESIGN.md), with the
 * page as a rounded panel on the gray page and the sidebar on its left.
 *
 * The shell navigates by label: a click hands us the label of the item, and `routes` below turns
 * it back into a URL. So every label in the sidebar is unique, and the URL stays the truth: what is
 * "active" is worked out from the location, never kept as state, which keeps links, reloads and the
 * back button honest.
 *
 * The sidebar has two faces. The app: the pages the owner works in, then every project as a folder,
 * and the open project's live tasks under it. Settings: the fleet's administration (agents, nodes,
 * the kit, secrets, habits), with a way back.
 *
 * On the right, the work area: a task opened from anywhere (a board card, a list row, the folder, ⌘K)
 * opens there beside the page, as `?task=<id>` in the URL (`peek.tsx`).
 */

/** What a sidebar row or a ⌘K command that opens a task beside the page hands back. */
const TASK = 'task:'

type Page = { label: string; path: string; icon: IconSvgElement; /** other paths that are this page */ match?: string[] }

const MAIN: Page[] = [
  { label: 'Board', path: '/', icon: KanbanIcon, match: ['/items/:id'] },
  { label: 'Inbox', path: '/inbox', icon: InboxIcon },
  { label: 'Roadmap', path: '/roadmap', icon: Calendar03Icon },
  { label: 'Tasks', path: '/tasks', icon: CheckListIcon, match: ['/tasks/:id', '/tasks/:id/session'] },
  { label: 'Sessions', path: '/sessions', icon: PlayCircleIcon },
  { label: 'People', path: '/people', icon: UserGroupIcon, match: ['/people/:id'] },
  { label: 'Retro', path: '/retro', icon: AiBrain01Icon },
]

const SETTINGS: Page[] = [
  { label: 'Agents', path: '/agents', icon: RoboticIcon },
  { label: 'Nodes', path: '/nodes', icon: HardDriveIcon, match: ['/nodes/:id'] },
  { label: 'Kit', path: '/kit', icon: PackageIcon, match: ['/kit/:id'] },
  { label: 'Secrets', path: '/secrets', icon: Key01Icon, match: ['/secrets/:id'] },
  { label: 'Habits', path: '/habits', icon: RepeatIcon },
]

const SETTINGS_LABEL = 'Settings'
const BACK_LABEL = 'Back to app'
const ALL_PROJECTS = 'All projects'

const isAt = (page: Page, pathname: string) =>
  [page.path, ...(page.match ?? [])].some((p) => matchPath({ path: p, end: true }, pathname))

/**
 * The page scrolls inside the shell's panel, not the window, so the browser's own scroll
 * restoration does nothing for it: going back from a task to a long list would land wherever the
 * panel was. This keeps the panel's position per URL, starts a new page at the top, and puts it
 * back on back/forward (waiting a moment for the page's data to make it tall enough). A filter
 * changing the query on the same page leaves the scroll alone.
 */
function usePanelScroll() {
  const { pathname, search } = useLocation()
  const navType = useNavigationType()
  const here = pathname + search
  const at = useRef(here)
  const saved = useRef(new Map<string, number>())
  at.current = here
  useEffect(() => {
    const main = document.querySelector<HTMLElement>('.fx-shell .ui-as-main')
    if (!main) return
    const onScroll = () => saved.current.set(at.current, main.scrollTop)
    main.addEventListener('scroll', onScroll, { passive: true })
    return () => main.removeEventListener('scroll', onScroll)
  }, [])
  useLayoutEffect(() => {
    const main = document.querySelector<HTMLElement>('.fx-shell .ui-as-main')
    if (!main) return
    const want = navType === 'POP' ? (saved.current.get(at.current) ?? 0) : 0
    main.scrollTop = want
    if (want === 0) return
    let tries = 0
    let frame = 0
    const settle = () => {
      main.scrollTop = want
      if (main.scrollTop < want - 1 && ++tries < 60) frame = requestAnimationFrame(settle)
    }
    frame = requestAnimationFrame(settle)
    return () => cancelAnimationFrame(frame)
    // Only a new page moves the scroll; `search` changing (a filter) does not.
  }, [pathname])
}

export function Shell() {
  const { pathname } = useLocation()
  const navigate = useNavigate()
  const peek = useTaskPeek()
  usePanelScroll()
  const { mode, setMode } = useTheme()
  // The sidebar's counts and folders. The overview is the landing data the board predates; one
  // call every 20 s is cheap, and a sidebar a few seconds behind is fine.
  const overview = useApi<Overview>('/api/overview', 20_000)
  const data = overview.data

  // Every project with its colour, for the pages: whatever names a project is marked with its hue.
  const projectList = useMemo(
    () => (data?.projects ?? []).map((p) => ({ slug: p.slug, name: p.name || p.slug, color: p.color })),
    [data?.projects],
  )

  const inSettings = SETTINGS.some((p) => isAt(p, pathname))
  // The task in view: the one open beside the page, or the one whose page this is.
  const taskId = peek.id ?? matchPath({ path: '/tasks/:id/*' }, pathname)?.params.id ?? null
  // The open folder: the project whose page this is, or the project of that task (so clicking a
  // task in the folder keeps the folder open, with that task marked).
  const projectSlug =
    matchPath({ path: '/projects/:slug/*' }, pathname)?.params.slug ??
    (taskId ? (data?.tasks.find((t) => t.id === taskId)?.project ?? null) : null)

  const model = useMemo(() => {
    // label -> path, for every item the shell can hand back to us
    const routes = new Map<string, string>()
    const take = (label: string, path: string) => {
      // A project named like a page (or like another project) gets a number, so no label is ambiguous.
      let l = label
      for (let n = 2; routes.has(l); n++) l = `${label} (${n})`
      routes.set(l, path)
      return l
    }
    routes.set(SETTINGS_LABEL, SETTINGS[0].path)
    routes.set(BACK_LABEL, '/')

    const counts: Record<string, number> = {
      // What waits on the owner: the board's first column.
      Board: data?.items.length ?? 0,
      Inbox: data?.todos_open ?? 0,
    }
    const pages = inSettings ? SETTINGS : MAIN
    const nav: ShellNavItem[] = pages.map((p) => ({ label: take(p.label, p.path), icon: p.icon, count: counts[p.label] }))
    if (!inSettings) nav.push({ label: SETTINGS_LABEL, icon: Settings01Icon })

    const groups: ShellGroup[] = []
    let activeProject: string | null = null
    let activeTask: string | null = null
    if (!inSettings) {
      const projects = (data?.projects ?? []).map((p) => {
        const label = take(p.name || p.slug, `/projects/${encodeURIComponent(p.slug)}`)
        if (p.slug === projectSlug) activeProject = label
        // The folder in its project's hue, as everything of that project is.
        return { label, icon: Folder01Icon, count: p.open, slug: p.slug, name: p.name || p.slug, color: `var(--fx-hue-${p.color})` }
      })
      groups.push({
        label: 'Projects',
        items: [{ label: take(ALL_PROJECTS, '/projects'), icon: FolderLibraryIcon }, ...projects],
        empty: overview.loading ? 'Loading…' : 'No projects yet',
      })
      // The open project, as a folder: its live work, one row per task.
      const open = projects.find((p) => p.slug === projectSlug)
      if (open) {
        const tasks = (data?.tasks ?? []).filter((t) => t.project === open.slug)
        groups.push({
          label: open.name,
          items: tasks.map((t) => {
            const label = take(`${t.key} ${t.title}`, `${TASK}${t.id}`)
            if (t.id === taskId) activeTask = label
            return { label, icon: t.running ? Loading03Icon : Task01Icon }
          }),
          empty: 'No open tasks',
        })
      }
    }

    const page = pages.find((p) => isAt(p, pathname))
    // A task in the open folder is marked there; its project only on the project's own pages.
    const onTaskPage = matchPath({ path: '/tasks/:id/*' }, pathname) !== null
    const active =
      activeTask ??
      (onTaskPage ? undefined : activeProject) ??
      (page ? page.label : matchPath({ path: '/projects', end: true }, pathname) ? ALL_PROJECTS : undefined)

    // ⌘K: every page, and the open tasks by their key and title. Projects are in its "Go to" group.
    const commands: CommandGroup[] = [
      {
        id: 'pages',
        label: 'Pages',
        commands: [...MAIN, ...SETTINGS].map((p) => ({
          id: `path:${p.path}`,
          label: p.label,
          icon: <Icon icon={p.icon} size={16} />,
        })),
      },
      {
        id: 'tasks',
        label: 'Open tasks',
        commands: (data?.tasks ?? []).map((t) => ({
          id: `${TASK}${t.id}`,
          label: `${t.key} · ${t.title}`,
          hint: t.project,
          keywords: `${t.id} ${t.project} ${t.status}`,
          icon: <Icon icon={t.running ? Loading03Icon : Task01Icon} size={16} />,
        })),
      },
    ]
    return { routes, nav, groups, active, commands }
  }, [data, overview.loading, inSettings, projectSlug, taskId, pathname])

  // A task from the folder or ⌘K: opened beside the page, or, already open, brought forward.
  const openTask = (id: string) => (id === peek.id ? ws.show('task') : peek.open(id))
  const go = (label: string) => {
    const to = model.routes.get(label)
    if (to?.startsWith(TASK)) openTask(to.slice(TASK.length))
    // The page already open: the click closes the task beside it, and keeps the page's filters.
    else if (to === pathname && peek.id) peek.close()
    else if (to && to !== pathname) navigate(to)
  }

  // The work area holds one tab, the open task. It is there only while a task is open, so a page
  // with none has no empty panel and no button for one.
  // The tab is named by the task's F-key: from the sidebar's data for an open task, or from the
  // panel itself once it has loaded one that is finished.
  const [loadedKey, setLoadedKey] = useState<{ id: string; key: string } | null>(null)
  const peekKey =
    data?.tasks.find((t) => t.id === peek.id)?.key ?? (loadedKey?.id === peek.id ? loadedKey.key : undefined)
  const ws = useWorkArea({
    tabs: peek.id
      ? [{ id: 'task', title: peekKey ?? peek.id, icon: Task01Icon, content: <TaskPeek key={peek.id} id={peek.id} onKey={(key) => setLoadedKey({ id: peek.id!, key })} /> }]
      : [],
    newTab: false,
  })
  // A task opened (or another one in its place) brings the panel forward, unfolding it if the
  // owner had folded it away.
  useEffect(() => {
    if (peek.id) ws.show('task')
    // `ws` is a new object every render; only the task changing should reveal it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [peek.id])
  // The tab's × closes the task: it leaves the URL too. (The panel's own fold button only folds it;
  // the task stays open behind the button that brings it back.)
  const tabs = ws.state.tabs.length
  const hadTab = useRef(false)
  useEffect(() => {
    if (hadTab.current && tabs === 0 && peek.id) {
      peek.close()
      // The shell would focus the button that brings the panel back, but with the task gone the
      // panel goes too; the page takes the focus rather than leave it nowhere.
      requestAnimationFrame(() => {
        const main = document.querySelector<HTMLElement>('.fx-shell .ui-as-main')
        if (!main) return
        if (!main.hasAttribute('tabindex')) main.setAttribute('tabindex', '-1')
        main.focus({ preventScroll: true })
      })
    }
    hadTab.current = tabs > 0
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tabs])

  const appearance: [Mode, string, IconSvgElement][] = [
    ['system', 'System', ComputerIcon],
    ['light', 'Light', Sun03Icon],
    ['dark', 'Dark', Moon02Icon],
  ]
  const accountItems: MenuItem[] = [
    {
      label: 'Appearance',
      icon: PaintBoardIcon,
      submenu: appearance.map(([m, label, icon]) => ({ label, icon, checked: m === mode, onSelect: () => setMode(m) })),
    },
    { label: SETTINGS_LABEL, icon: Settings01Icon, divider: true, onSelect: () => navigate(SETTINGS[0].path) },
  ]
  const workspaceItems: MenuItem[] = [
    { label: 'Board', icon: KanbanIcon, onSelect: () => navigate('/') },
    { label: ALL_PROJECTS, icon: FolderLibraryIcon, onSelect: () => navigate('/projects') },
    { label: SETTINGS_LABEL, icon: Settings01Icon, onSelect: () => navigate(SETTINGS[0].path) },
  ]

  return (
    <RevealTask.Provider value={() => ws.show('task')}>
    <div className="fx-shell">
      <style href="fx-shell" precedence="default">
        {css}
      </style>
      <AppShell
        layout="inset"
        workspace="Fleet"
        workspaceMeta={data?.release ? `Release ${data.release}` : 'Agent fleet'}
        workspaceItems={workspaceItems}
        user={{ name: 'Owner', email: 'Signed in with a link from Telegram' }}
        accountItems={accountItems}
        nav={model.nav}
        projects={[]}
        groups={model.groups}
        active={model.active}
        onNavigate={go}
        back={inSettings ? { title: SETTINGS_LABEL, label: BACK_LABEL } : undefined}
        sideCards={false}
        commands={model.commands}
        onCommand={(id) => {
          if (id.startsWith('path:')) navigate(id.slice('path:'.length))
          else if (id.startsWith(TASK)) openTask(id.slice(TASK.length))
        }}
        workArea={peek.id ? ws : undefined}
        panelTitle="Task panel"
      >
        <ProjectsProvider projects={projectList}>
          <Outlet />
        </ProjectsProvider>
      </AppShell>
    </div>
    </RevealTask.Provider>
  )
}

// The shell fills the window and the page scrolls inside its panel (`.ui-as-main`), so the window
// itself never scrolls. Up to 900px of app width the sidebar becomes a drawer, and the button that
// opens it sits in the panel's top left corner: the page starts below it there, as affectUI's own
// screens do (`.ui-as-page`).
const css = /* css */ `
.fx-shell {
  height: 100dvh;
}
@container ui-as (max-width: 900px) {
  .ui-as-main > .fx-page {
    padding-top: 52px;
  }
}
`
