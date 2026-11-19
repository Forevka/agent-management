import React from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Route, Routes } from 'react-router-dom'
import { Theme } from '@radix-ui/themes'
import '@radix-ui/themes/styles.css'
import './app.css'

import { Shell } from './shell'
import Dashboard from './pages/Dashboard'
import Tasks from './pages/Tasks'
import TaskDetail from './pages/TaskDetail'
import Sessions from './pages/Sessions'
import Projects from './pages/Projects'
import ProjectDetail from './pages/ProjectDetail'
import Secrets from './pages/Secrets'
import SecretFolder from './pages/SecretFolder'
import Nodes from './pages/Nodes'
import NodeDetail from './pages/NodeDetail'
import Agents from './pages/Agents'
import Inbox from './pages/Inbox'
import Item from './pages/Item'

createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    {/* The theme follows the operating system, which is what the old stylesheet did too.
        `grayColor=sand` and the indigo accent keep the fleet's own quiet palette. */}
    <Theme accentColor="indigo" grayColor="sand" radius="medium" appearance="inherit">
      {/* The v7 behaviours, opted into now: they are what this app already expects, and it
          keeps the console clean so a real warning stands out. */}
      <BrowserRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <Routes>
          <Route element={<Shell />}>
            <Route path="/" element={<Dashboard />} />
            <Route path="/tasks" element={<Tasks />} />
            <Route path="/tasks/:id" element={<TaskDetail />} />
            <Route path="/sessions" element={<Sessions />} />
            <Route path="/projects" element={<Projects />} />
            <Route path="/projects/:slug" element={<ProjectDetail />} />
            <Route path="/secrets" element={<Secrets />} />
            <Route path="/secrets/:id" element={<SecretFolder />} />
            <Route path="/nodes" element={<Nodes />} />
            <Route path="/nodes/:id" element={<NodeDetail />} />
            <Route path="/agents" element={<Agents />} />
            <Route path="/inbox" element={<Inbox />} />
            <Route path="/items/:id" element={<Item />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </Theme>
  </React.StrictMode>,
)
