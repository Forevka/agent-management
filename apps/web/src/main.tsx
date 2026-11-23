import React from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Route, Routes } from 'react-router-dom'
import './aui.css'
import { startTheme } from '@/components/fx/theme'

import { Shell } from './shell'
import BoardPage from './pages/Board'
import Tasks from './pages/Tasks'
import TaskDetail from './pages/TaskDetail'
import Sessions from './pages/Sessions'
import Session from './pages/Session'
import Projects from './pages/Projects'
import {
  ProjectAgents,
  ProjectLayout,
  ProjectOverview,
  ProjectSessions,
  ProjectSettings,
  ProjectTasks,
} from './pages/Project'
import Secrets from './pages/Secrets'
import SecretFolder from './pages/SecretFolder'
import Nodes from './pages/Nodes'
import NodeDetail from './pages/NodeDetail'
import Agents from './pages/Agents'
import Inbox from './pages/Inbox'
import Habits from './pages/Habits'
import Retro from './pages/Retro'
import People from './pages/People'
import Person from './pages/Person'
import Kit from './pages/Kit'
import KitItem from './pages/KitItem'
import Item from './pages/Item'

// Before the first paint: the owner's choice of light or dark is on <html> already, so the page
// never flashes the wrong one (see components/fx/theme.tsx).
startTheme()

createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    {/* Without transitions: a search field here is driven by the URL, and a controlled input
        whose value arrives in a transition loses keystrokes and its cursor. */}
    <BrowserRouter useTransitions={false}>
      <Routes>
        <Route element={<Shell />}>
          <Route path="/" element={<BoardPage />} />
          <Route path="/tasks" element={<Tasks />} />
          <Route path="/tasks/:id" element={<TaskDetail />} />
          <Route path="/tasks/:id/session" element={<Session />} />
          <Route path="/sessions" element={<Sessions />} />
          <Route path="/projects" element={<Projects />} />
          {/* A project is a directory: each section its own URL, under one header and its tabs. */}
          <Route path="/projects/:slug" element={<ProjectLayout />}>
            <Route index element={<ProjectOverview />} />
            <Route path="tasks" element={<ProjectTasks />} />
            <Route path="sessions" element={<ProjectSessions />} />
            <Route path="agents" element={<ProjectAgents />} />
            <Route path="settings" element={<ProjectSettings />} />
          </Route>
          <Route path="/secrets" element={<Secrets />} />
          <Route path="/secrets/:id" element={<SecretFolder />} />
          <Route path="/nodes" element={<Nodes />} />
          <Route path="/nodes/:id" element={<NodeDetail />} />
          <Route path="/agents" element={<Agents />} />
          <Route path="/inbox" element={<Inbox />} />
          <Route path="/habits" element={<Habits />} />
          <Route path="/retro" element={<Retro />} />
          <Route path="/people" element={<People />} />
          <Route path="/people/:id" element={<Person />} />
          <Route path="/kit" element={<Kit />} />
          <Route path="/kit/:id" element={<KitItem />} />
          <Route path="/items/:id" element={<Item />} />
        </Route>
      </Routes>
    </BrowserRouter>
  </React.StrictMode>,
)
