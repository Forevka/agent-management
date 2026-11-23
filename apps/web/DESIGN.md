# The fleet's UI: affectUI is the golden standard

Every component, page and style decision in `apps/web` follows **[affectUI](https://www.affectui.com)**
— an agent-ready component library for AI products. Read this before writing a line of UI here,
and prefer *their* answer over inventing one. When something is missing from their free set, the
rule is not "do it however": it is "build it the way they would have", to the contract below.

Their material, in order of usefulness:

| What | Where |
|---|---|
| The catalogue (232 items: components, screens, blocks, illustrations, motion) | <https://www.affectui.com/docs> |
| **Everything in one text file, for an agent to read** | <https://www.affectui.com/llms.txt> (and `llms-full.txt`) |
| Theming, accessibility, agent notes | `/docs/theming`, `/docs/accessibility`, `/docs/ai` |
| One component's source, as JSON | `https://www.affectui.com/r/<slug>.json` |
| MCP server (search, docs, source) | `https://www.affectui.com/api/mcp` |

**Screens and blocks are the reference for how a page should read**, not just how a control should
look: generous spacing, one near-black primary, 1px borders, soft shadows, 10px radii, quiet
colour used only to mean something. When a new page is needed, look at their closest screen first.

## Licence: free items are committed, Pro items never are

**Some items are free and MIT; the rest are affectUI Pro**, which the fleet owns (a one-time
licence). The two are handled differently, because **this repository is public**: committing a Pro
file would publish what the licence sells.

- **Free items** live in `components/ui/`, committed, each exactly as their registry serves it — do
  not reformat them, and **keep the Hugeicons (MIT) credit comments** some of them carry. (The
  session page's tool calls are their Tool Call Card, its "working" state their Thinking Loader.)
- **Pro items are fetched at build time** by `scripts/affectui.mjs`, which runs before every
  `npm run dev`, `build` and `check`, and **never enter `src/`**:
  - `affectui.lock.json` (committed) lists every item we use, free or Pro, with the SHA-256 of
    each file (over the text with CRLF as LF, since a Windows checkout adds CRLF). The registry
    has no versions, so the lock is what pins us: a file that changed upstream fails the build
    until `npm run aui:update` is run and the change is read.
  - A Pro file is downloaded with the owner's access key into a cache **outside the repository**
    (`AFFECTUI_CACHE`, by default `~/.cache/affectui`; CI caches it per lock, on pushes only), and
    the copy the app compiles — with our patches applied — goes to `node_modules/.affectui/`,
    which git has ignored on every commit there ever was. So no branch, old or new, and no
    `git add -A` can pick a Pro file up.
  - `@/components/ui/<name>` resolves to `src/components/ui/` and, failing that, to
    `node_modules/.affectui/` (a small resolver in `vite.config.ts`, two `paths` in `tsconfig.json`),
    so a Pro file is imported like a committed one.
  - The key is `AFFECTUI_TOKEN` in the environment, `AFFECTUI_TOKEN_FILE`, or the Docker build
    secret `affectui`. It is needed only when the cache does not have a file yet; without it the
    build says so and stops. CI has it as a repository secret; the VPS as a root-only file
    (`deploy/README.md` §8); a fleet agent in `fleet-src` runs the build under `fleet secret exec`.
    The Docker build deletes the Pro source in the same step, so no image layer holds it.
  - `npm run aui:check` (in CI) fails if any tracked file is a Pro file, by name or by content.
- **Patches** (`scripts/affectui.patches.mjs`) are exact replacements the script applies to a Pro
  file on every build, each required to match exactly once. They exist only to give a component a
  prop it lacks where it hard-codes showcase content: App Shell, for instance, ships sample
  workspace, account and ⌘K menus ("Billing", "Log out"), and the patches let us pass our own.
  They never change what a component does with its data, and never copy a block of it (that file is
  public).

Add an item with `npm run aui:update -- <slug>` (it resolves its registry dependencies too), then
commit the lock and the free files. Its npm dependencies (for most Pro
items, `@hugeicons/react` and `@hugeicons/core-free-icons`, MIT and bundled, so the page still
works offline) must be in `package.json` at an exact version; the script says which are missing.

**Pro screens are references, not components here.** Every affectUI screen (Kanban, Agent / Tasks,
Inbox, Settings…) renders its own App Shell with its own hard-coded sidebar, so none of them can sit
inside our navigation. We use **App Shell** itself (`shell.tsx`) and Pro *components* (data-driven:
they take our data as props), and build each page in the layout of the matching screen.

Ours, in `components/fx/`, written to the same contract: `fx/table.tsx` and `fx/stat.tsx` (until
Pro's Data Table and Stat Card replace them), `fx/board.tsx` (the landing page's columns and cards —
a *reading* board, not a planning one: the cards do not drag, because a column is the task's real
state in the fleet and moving one by hand would be a lie), `fx/layout.tsx` (`Page`, `PageHeader`,
`Section`, `Stack`, `Row`, `Grid`, `Text`, `Heading`), `fx/note.tsx` (a callout), `fx/radio.tsx`,
`fx/theme.tsx` (dark mode), `fx/icons.tsx` (inline icons — see "No external anything" below).

## The frame: App Shell

`src/shell.tsx` puts every page inside affectUI's App Shell (Pro), in its inset layout: the page is
a rounded panel on the gray page, the sidebar on its left, and the page scrolls inside its panel
(`.ui-as-main`) — the window never scrolls. Things that follow from that:

- **The shell navigates by label.** A click hands `shell.tsx` an item's label, and a table there
  turns it back into a URL, so every label in the sidebar is unique (a clash gets a number). What
  is active is worked out from the URL, never kept as state.
- **Two faces.** The app (Board, Inbox, Tasks, Sessions, People, Retro, then **every project as a
  folder**, and the open project's live tasks under it), and Settings (Agents, Nodes, Kit, Secrets,
  Habits) with a way back. A page belongs to Settings by its path.
- **The panel is a size container** (`ui-as-main`). Something that must span the panel rather than
  the page's reading column (the board) uses `cqw`, not `vw`; a scroll listener listens on the
  panel, not the window. Up to 900px of app width the sidebar is a drawer and its button sits in
  the panel's top left corner, so a page starts 52px down there.
- **Dark mode** is still `fx/theme.tsx`: the account menu's Appearance sets it, and App Shell's
  dark rules key on the same `data-theme`.

## The contract every component here follows

Read `components/ui/button.tsx` once — it is the clearest example of all ten.

1. **One file per component.** The component and its styles, nothing else. No stylesheet, no
   CSS-in-JS runtime, no animation library, **no Tailwind**.
2. **Styles are injected beside the component**, with React 19's deduplicating style hoist:
   `<style href="ui-button" precedence="default">{css}</style>`. The `href` is the identity — one
   per component, so a hundred buttons inject one stylesheet.
3. **`.ui-<name>` for theirs, `.fx-<name>` for ours**, with `__element` for parts
   (`.ui-button__label`). Two namespaces so a vendored update can never collide with our code.
4. **Variants are `data-*` attributes**, not class permutations: `data-variant`, `data-size`,
   `data-tone`, `data-loading`. The CSS selects on them.
5. **Colour comes from a token, never a literal.** A component declares its own variables and
   reads page-level tokens with a fallback:
   `--ui-btn-ink: var(--aui-ink, #171717)`. The fallback is what the component looks like with no
   theme at all; `src/aui.css` is the theme.
6. **Sizes are local variables** (`--h`, `--px`, `--gap`, `--fs`, `--icon`, `--radius`) set per
   `data-size`, so one rule block describes every size.
7. **Dark mode is `:where(.dark, [data-theme="dark"]) .ui-x { … }`** — an ancestor class or
   attribute, **never** `@media (prefers-color-scheme)`. That is what lets the account menu's
   Appearance override the system (`fx/theme.tsx` writes `data-theme` on `<html>`).
8. **Reduced motion is honoured** in every component that moves: wrap the animation in
   `@media (prefers-reduced-motion: reduce)` and turn it off.
9. **An unknown prop value falls back to the default** rather than rendering something unstyled
   (`VARIANTS.includes(variant) ? variant : "primary"`).
10. **React 19 idiom**: `ref` is an ordinary prop (no `forwardRef`), and `ComponentPropsWithRef<"x">`
    is spread onto the element so every native attribute keeps working.

## The token layer

`src/aui.css` is the whole theme: the `--aui-*` tokens in light, the same set under
`[data-theme="dark"]`, the base element styles, and three app classes (`.fx-link`, `.fx-item`).
Nothing else in the app sets a colour. The values are affectUI's own defaults, with two departures,
both commented in the file:

- `--aui-muted` is `#737373`, not their `#8f8f8f`: at 2.9:1 on white theirs fails WCAG AA for the
  small text this app is mostly made of.
- No webfont. `index.html` says the page must work on a machine with no internet (the orchestrator
  serves it), so Inter is used when installed and the system stack otherwise.

## Adding or updating a component

```sh
cd apps/web
npm run aui:update -- <slug>      # add an item (free or Pro) and its registry dependencies
npm run aui:update                # re-fetch everything in the lock, and rewrite it
```

Then read the diff: `git diff` shows the free files; for a Pro file, copy it out of the cache
first, since the update overwrites it in place. A change upstream may not agree with our tokens
or our patches. The items vendored before
the lock existed (the 32 plain free ones, like `button.tsx`) are not in it; updating one of those is
still `npx shadcn@latest add @affectui/<slug>`.

## Rules that outrank "make it look nice"

- **Untrusted text stays text.** Slack, Jira, agent output, a retrospective's proposal: all of it
  is written by someone who is not the owner. `dangerouslySetInnerHTML` is forbidden anywhere in
  `apps/web`, and a test in `crates/orchestrator/src/web.rs` fails the build if it appears in
  `src/`. Pro files are not in `src/`, so `scripts/affectui.mjs` holds them to the same rule: it
  refuses any that uses it, except an item listed in its `HTML_ALLOWED` with the reason (the lock
  records it). The one so far is App Shell, which builds the HTML of a work-area `doc` artifact
  itself, escaping its title, paragraphs and bullets — and putting `doc.html` in raw. **So never
  pass a `doc` artifact an `html` field.**
- **No external anything.** No CDN, no Google Fonts, no remote icon set. The binary serves this
  page and must work offline.
- **Every page is a URL**, filters included (`/tasks?project=bioins&status=failed`), so the
  orchestrator can send the owner a link to exactly what it is talking about.
- **One accent.** affectUI's palette has a single near-black primary; colour means status
  (success, warning, danger, info), not decoration. Resist adding a brand hue.
- **`npm ci && npm run build` before any Rust build that must serve the UI** — `build.rs` embeds a
  placeholder page when `dist` is missing.
