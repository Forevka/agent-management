import { Box, Container, Flex, Text } from '@radix-ui/themes'
import { NavLink, Outlet } from 'react-router-dom'

const NAV = [
  ['/', 'Dashboard'],
  ['/tasks', 'Tasks'],
  ['/sessions', 'Sessions'],
  ['/projects', 'Projects'],
  ['/inbox', 'Inbox'],
  ['/habits', 'Habits'],
  ['/agents', 'Agents'],
  ['/nodes', 'Nodes'],
  ['/secrets', 'Secrets'],
] as const

/// The frame: a sticky header with the nav, and the page under it. One place decides the
/// page width, so every page lines up.
export function Shell() {
  return (
    <>
      <Box className="fleet-header">
        <Container size="4" px="4">
          <Flex align="center" gap="4" py="3">
            <Text weight="bold" size="3">
              Fleet
            </Text>
            <Flex className="fleet-nav" gap="1" align="center" asChild>
              <nav>
                {NAV.map(([to, label]) => (
                  // `end` so "Dashboard" is only current on "/" itself.
                  <NavLink key={to} to={to} end={to === '/'}>
                    <Text size="2">{label}</Text>
                  </NavLink>
                ))}
              </nav>
            </Flex>
          </Flex>
        </Container>
      </Box>
      <Container size="4" px="4" pb="9">
        <Outlet />
      </Container>
    </>
  )
}
