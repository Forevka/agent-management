// Patches to affectUI Pro files, applied by scripts/affectui.mjs to the registry's text on every
// build. Each is [the exact text to find, what replaces it], and must match exactly once; a patch
// that stops matching fails the build, so an upstream change is read rather than lost.
//
// Keep them few and small: they add a prop where a component hard-codes sample content, and
// never change what a component does with its own data. Only the needle and its replacement live
// here, never a copied block of the component (this file is public; the component is not).

export const PATCHES = {
  // App Shell ships the sample menus of its showcase (Invite people, Billing, Language, Log out,
  // and a ⌘K menu of made-up commands), with no prop to replace them. These give it four:
  // workspaceItems and accountItems replace the two menus, commands replaces the ⌘K menu's sample
  // groups (its "Go to" group stays), and onCommand is told when one of ours is run.
  'app-shell.tsx': [
    [
      '  className?: string;\n};\n\nexport function AppShell({',
      '  className?: string;\n' +
        '  /** (fleet patch) The workspace menu, in place of the sample one */\n' +
        '  workspaceItems?: MenuItem[];\n' +
        '  /** (fleet patch) The account menu, in place of the sample one */\n' +
        '  accountItems?: MenuItem[];\n' +
        '  /** (fleet patch) The search menu\'s groups after "Go to", in place of the samples */\n' +
        '  commands?: CommandGroup[];\n' +
        '  /** (fleet patch) A command of `commands` was run */\n' +
        '  onCommand?: (id: string, query?: string) => void;\n' +
        '};\n\nexport function AppShell({',
    ],
    [
      '  rail: railNav,\n}: AppShellProps) {',
      '  rail: railNav,\n  workspaceItems: givenWorkspaceItems,\n  accountItems,\n  commands,\n  onCommand,\n}: AppShellProps) {',
    ],
    ['  const workspaceItems: MenuItem[] = [', '  const workspaceItems: MenuItem[] = givenWorkspaceItems ?? ['],
    [
      '      items={[\n        {\n          label: "Language",',
      '      items={accountItems ?? [\n        {\n          label: "Language",',
    ],
    [
      'const commandGroups = defaultGroups.map((g) => (g.id === "go" ? goTo : g));',
      'const commandGroups = commands ? [goTo, ...commands] : defaultGroups.map((g) => (g.id === "go" ? goTo : g));',
    ],
    [
      '            onRun={(id) => {\n              closeSearch();\n              if (id.startsWith("go:")) go(id.split(":").slice(2).join(":"));',
      '            onRun={(id, query) => {\n              closeSearch();\n              if (id.startsWith("go:")) go(id.split(":").slice(2).join(":"));\n              else onCommand?.(id, query);',
    ],
    // MenuButton's button is named by its current choice alone ("failed", "bioins"), which tells a
    // screen reader nothing about which filter it is. `label` names it: "Status: failed".
    [
      '  className,\n}: {\n  value: T;\n  options: { value: T; label: string }[];',
      '  className,\n  label,\n}: {\n' +
        '  /** (fleet patch) What the menu chooses, e.g. "Status": with the choice, its accessible name */\n' +
        '  label?: string;\n  value: T;\n  options: { value: T; label: string }[];',
    ],
    [
      'aria-expanded={open} onClick={() => toggle()}>',
      'aria-expanded={open} aria-label={label ? `${label}: ${current?.label ?? ""}` : undefined} onClick={() => toggle()}>',
    ],
  ],
}
