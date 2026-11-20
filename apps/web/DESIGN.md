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

## Licence, and what that costs us

**74 of the 232 items are free and MIT; the other 158 are affectUI Pro** (a one-time purchase from
$249). We use the free ones. `components/ui/` holds the 30 we vendored, each exactly as their
registry serves it — do not reformat them, and **keep the Hugeicons (MIT) credit comments** some of
them carry.

Four things we wanted are Pro, so they are ours, in `components/fx/`, written to the same contract:

| Theirs (Pro) | Ours |
|---|---|
| Data Table | `fx/table.tsx` — `Table` (declarative columns) and `Facts` |
| Stat Card | `fx/stat.tsx` — `Stat` |
| Toaster, Command Menu | not built; nothing needs them yet |

Also ours, because the free set has no equivalent: `fx/layout.tsx` (`Page`, `PageHeader`, `Section`,
`Stack`, `Row`, `Grid`, `Text`, `Heading`), `fx/note.tsx` (a callout), `fx/radio.tsx`, `fx/theme.tsx`
(dark mode), `fx/icons.tsx` (inline icons — see "No external anything" below).

If Pro is ever bought: put the key in `AFFECTUI_TOKEN` and the same `npx shadcn` command installs
Pro items too; then `fx/table.tsx` and `fx/stat.tsx` can be retired in favour of theirs.

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
   attribute, **never** `@media (prefers-color-scheme)`. That is what lets the header's toggle
   override the system (`fx/theme.tsx` writes `data-theme` on `<html>`).
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
# One of the free items, by slug (see llms.txt for the list and the free/Pro marks):
cd apps/web
npx shadcn@latest add https://www.affectui.com/r/<slug>.json
# or, via the registry alias already in components.json:
npx shadcn@latest add @affectui/<slug>
```

It lands in `src/components/ui/<slug>.tsx` and imports only `react`. The registry answers **401**
for a Pro item, which is how you find out a slug is not free.

Updating is the same command again, then read the diff: these files are ours now, and a change
upstream may not agree with our tokens.

## Rules that outrank "make it look nice"

- **Untrusted text stays text.** Slack, Jira, agent output, a retrospective's proposal: all of it
  is written by someone who is not the owner. `dangerouslySetInnerHTML` is forbidden anywhere in
  `apps/web`, and a test in `crates/orchestrator/src/web.rs` fails the build if it appears.
- **No external anything.** No CDN, no Google Fonts, no remote icon set. The binary serves this
  page and must work offline.
- **Every page is a URL**, filters included (`/tasks?project=bioins&status=failed`), so the
  orchestrator can send the owner a link to exactly what it is talking about.
- **One accent.** affectUI's palette has a single near-black primary; colour means status
  (success, warning, danger, info), not decoration. Resist adding a brand hue.
- **`npm ci && npm run build` before any Rust build that must serve the UI** — `build.rs` embeds a
  placeholder page when `dist` is missing.
