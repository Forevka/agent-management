#!/usr/bin/env node
// affectUI's registry, at build time.
//
// The fleet owns affectUI Pro, but this repository is public, so Pro source must never be
// committed: publishing it would give away what the licence sells. Every item this app uses from
// the registry is listed in `affectui.lock.json` with the SHA-256 of each file. Free items (MIT)
// are committed in `src/components/ui/`, like the ones vendored before Pro was bought. Pro items
// never enter `src/` at all:
//
//   - the registry's text is downloaded with the owner's access key into a cache outside the
//     repository (AFFECTUI_CACHE, by default ~/.cache/affectui; CI keeps it per lock), and
//   - the copy the app compiles is that text with our patches applied (`affectui.patches.mjs`:
//     exact replacements that give a component a prop it lacks), written to
//     `node_modules/.affectui/`, which every commit of this repository has always ignored. So
//     switching to an older branch, or `git add -A` anywhere, can never pick a Pro file up.
//
// `vite.config.ts` and `tsconfig.json` resolve `@/components/ui/<name>` to `src/components/ui/` and,
// failing that, to `node_modules/.affectui/`, so a Pro file is imported like any other.
//
// The registry serves only the latest version of an item, so the lock is what pins us: a file that
// changed upstream fails the build until someone runs `npm run aui:update` and looks at the change.
// Hashes are taken over the text with CRLF turned into LF, because a Windows checkout
// (core.autocrlf) hands the committed free files back with CRLF.
//
//   node scripts/affectui.mjs            make every locked file present and verified (prebuild)
//   node scripts/affectui.mjs --update [slug ...]
//                                        re-resolve the roots (plus any slugs given) and their
//                                        registry dependencies, then rewrite the lock
//   node scripts/affectui.mjs --check    fail if a Pro file is tracked by git (CI)
//
// The key comes from AFFECTUI_TOKEN, from the file AFFECTUI_TOKEN_FILE names, or from the Docker
// build secret /run/secrets/affectui. It is only needed when a Pro file is not in the cache yet.

import { createHash } from 'node:crypto'
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, writeFileSync, mkdirSync, renameSync, rmSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join, basename } from 'node:path'
import { fileURLToPath } from 'node:url'
import { PATCHES } from './affectui.patches.mjs'

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..')
const LOCK = join(ROOT, 'affectui.lock.json')
const UI = join(ROOT, 'src', 'components', 'ui')
const PRO_OUT = join(ROOT, 'node_modules', '.affectui')
const CACHE = process.env.AFFECTUI_CACHE?.trim() || join(homedir(), '.cache', 'affectui')
const REGISTRY = 'https://www.affectui.com/r'
const SLUG = /^[a-z0-9][a-z0-9-]*$/
const FILE = /^[a-z0-9][a-z0-9-]*\.(tsx|ts|css)$/

// The UI never sets HTML from a string: a Rust test (crates/orchestrator/src/web.rs) walks src/
// for `dangerouslySetInnerHTML`. Pro files are not in src/, so it is this script that holds them
// to the rule: an item may only do it when it is listed here with the reason it is safe *as the
// fleet uses it*, and the lock carries that reason.
const HTML_ALLOWED = {
  'app-shell':
    'renders a "doc" artifact of the work area as HTML it builds itself, escaping title, ' +
    'paragraphs and bullets; only doc.html goes in raw, and the fleet never passes doc.html',
}

const fail = (msg) => {
  console.error(`affectui: ${msg}`)
  process.exit(1)
}
const lf = (text) => text.replace(/\r\n/g, '\n')
const sha256 = (text) => createHash('sha256').update(lf(text), 'utf8').digest('hex')
const read = (path) => (existsSync(path) ? readFileSync(path, 'utf8') : undefined)

function readLock() {
  if (!existsSync(LOCK)) return { roots: [], items: {} }
  return JSON.parse(readFileSync(LOCK, 'utf8'))
}

function writeAtomic(path, text) {
  if (read(path) === text) return
  mkdirSync(dirname(path), { recursive: true })
  // `.tmp` last, so the repository's `*.tmp` rule covers a leftover too.
  const tmp = `${path}.${process.pid}.tmp`
  try {
    writeFileSync(tmp, text, 'utf8')
    renameSync(tmp, path)
  } finally {
    rmSync(tmp, { force: true })
  }
}

function token() {
  if (process.env.AFFECTUI_TOKEN?.trim()) return process.env.AFFECTUI_TOKEN.trim()
  for (const file of [process.env.AFFECTUI_TOKEN_FILE, '/run/secrets/affectui']) {
    const t = file && read(file)?.trim()
    if (t) return t
  }
  return undefined
}

async function fetchItem(slug, { needToken }) {
  if (!SLUG.test(slug)) fail(`not a registry slug: ${JSON.stringify(slug)}`)
  const key = token()
  if (needToken && !key) {
    fail(
      `${slug} is affectUI Pro and is not in the cache (${CACHE}). Set AFFECTUI_TOKEN (the access ` +
        `key from https://www.affectui.com/account/downloads), point AFFECTUI_TOKEN_FILE at a file ` +
        `holding it, or mount it as the Docker build secret "affectui". Pro source is never ` +
        `committed (see apps/web/DESIGN.md).`,
    )
  }
  const headers = needToken ? { Authorization: `Bearer ${key}` } : {}
  let res
  try {
    res = await fetch(`${REGISTRY}/${slug}.json`, { headers })
  } catch (e) {
    fail(`could not reach the affectUI registry for ${slug}: ${e.message}`)
  }
  if (res.status === 401 && !needToken) return { pro: true }
  if (!res.ok) {
    fail(
      res.status === 401
        ? `the registry refused the access key for ${slug} (401): the key is wrong or revoked`
        : `the registry answered ${res.status} for ${slug}`,
    )
  }
  return { pro: needToken, item: await res.json() }
}

/** The files an item contributes, by name. */
function filesOf(slug, item) {
  if (!item || !Array.isArray(item.files) || item.files.length === 0) fail(`${slug} has no files`)
  return item.files.map((f) => {
    const name = basename(f.path ?? '')
    if (!FILE.test(name)) fail(`${slug} has an unexpected file ${f.path}`)
    if (typeof f.content !== 'string') fail(`${slug}/${name} has no content`)
    // Say which item brought it in, and refuse it, rather than let it in unread.
    if (f.content.includes('dangerouslySetInnerHTML') && !HTML_ALLOWED[slug]) {
      fail(
        `${slug}/${name} uses dangerouslySetInnerHTML, which this app never allows. Read how; ` +
          `if untrusted text can never reach it, add ${slug} to HTML_ALLOWED with the reason.`,
      )
    }
    return { name, content: f.content }
  })
}

/** A Pro file as the app uses it: the registry's text with each patch applied exactly once. */
function patched(name, text) {
  for (const [i, [find, replace]] of (PATCHES[name] ?? []).entries()) {
    const at = text.indexOf(find)
    if (at < 0 || text.indexOf(find, at + 1) >= 0) {
      fail(
        `patch ${i + 1} for ${name} ${at < 0 ? 'no longer applies' : 'matches more than once'}: ` +
          `affectUI changed the file. Update scripts/affectui.patches.mjs.`,
      )
    }
    text = text.slice(0, at) + replace + text.slice(at + find.length)
  }
  return text
}

function checkDependencies(lock) {
  const pkg = JSON.parse(readFileSync(join(ROOT, 'package.json'), 'utf8'))
  const have = { ...pkg.dependencies, ...pkg.devDependencies }
  const missing = [...new Set(Object.values(lock.items).flatMap((i) => i.dependencies ?? []))]
    // "pkg@1.2" and "@scope/pkg@1.2" name the package without the version; "@scope/pkg" is whole.
    .map((d) => d.replace(/(.)@[^/]*$/, '$1'))
    .filter((d) => !have[d])
  if (missing.length) fail(`affectUI items need ${missing.join(', ')}; add them to package.json (exact versions)`)
}

const proFiles = (lock) =>
  Object.values(lock.items)
    .filter((i) => i.tier === 'pro')
    .flatMap((i) => Object.keys(i.files))

function checkLock(lock) {
  const pro = new Set(proFiles(lock))
  for (const [slug, entry] of Object.entries(lock.items)) {
    if (!SLUG.test(slug) || !['free', 'pro'].includes(entry.tier)) fail(`the lock's entry ${slug} is malformed`)
    for (const name of Object.keys(entry.files)) if (!FILE.test(name)) fail(`the lock names a bad file ${name}`)
  }
  for (const name of Object.keys(PATCHES)) {
    if (!pro.has(name)) fail(`scripts/affectui.patches.mjs patches ${name}, which is not a locked Pro file`)
  }
  for (const name of pro) {
    if (existsSync(join(UI, name))) fail(`src/components/ui/${name} is a Pro file; it must not be in src/ (delete it)`)
  }
}

/** Where the verified registry text of a file lives: the cache for Pro, the committed file for free. */
const sourcePath = (tier, name) => join(tier === 'pro' ? CACHE : UI, name)

function writePro(lock) {
  for (const name of proFiles(lock)) {
    writeAtomic(join(PRO_OUT, name), patched(name, readFileSync(join(CACHE, name), 'utf8')))
  }
}

async function ensure() {
  const lock = readLock()
  checkLock(lock)
  for (const [slug, entry] of Object.entries(lock.items)) {
    const stale = Object.entries(entry.files).filter(([name, hash]) => {
      const text = read(sourcePath(entry.tier, name))
      return text === undefined || sha256(text) !== hash
    })
    if (stale.length === 0) continue
    // A committed free file that differs was edited by hand, or the lock is out of date; either
    // way a download would silently undo something, so say so instead.
    if (entry.tier === 'free') {
      const changed = stale.filter(([name]) => existsSync(join(UI, name))).map(([n]) => n)
      if (changed.length) fail(`${changed.join(', ')} differ from the lock; run npm run aui:update and review`)
    }
    const got = await fetchItem(slug, { needToken: entry.tier === 'pro' })
    if (got.pro !== (entry.tier === 'pro')) fail(`${slug} is no longer ${entry.tier} upstream; run npm run aui:update`)
    const files = new Map(filesOf(slug, got.item).map((f) => [f.name, f.content]))
    for (const [name, want] of Object.entries(entry.files)) {
      const content = files.get(name)
      if (content === undefined) fail(`affectUI no longer serves ${name} in ${slug}; run npm run aui:update and review`)
      if (sha256(content) !== want) {
        fail(`affectUI changed ${slug} (${name}) since it was locked; run npm run aui:update and review the change`)
      }
      writeAtomic(sourcePath(entry.tier, name), content)
    }
    console.log(`affectui: fetched ${slug}`)
  }
  writePro(lock)
  checkDependencies(lock)
}

async function update(extra) {
  const lock = readLock()
  const roots = [...new Set([...(lock.roots ?? []), ...extra])].sort()
  if (roots.length === 0) fail('no roots: name the items to add, e.g. npm run aui:update -- app-shell')
  const items = {}
  const queue = [...roots]
  while (queue.length) {
    const slug = queue.shift()
    if (items[slug]) continue
    let got = await fetchItem(slug, { needToken: false })
    if (got.pro) got = await fetchItem(slug, { needToken: true })
    const tier = got.pro ? 'pro' : 'free'
    const files = {}
    let html = false
    for (const { name, content } of filesOf(slug, got.item)) {
      files[name] = sha256(content)
      html ||= content.includes('dangerouslySetInnerHTML')
      writeAtomic(sourcePath(tier, name), content)
    }
    items[slug] = {
      tier,
      files,
      ...(got.item.dependencies?.length ? { dependencies: got.item.dependencies } : {}),
      ...(html ? { html: HTML_ALLOWED[slug] } : {}),
    }
    for (const dep of got.item.registryDependencies ?? []) {
      const m = /^@affectui\/([a-z0-9-]+)$/.exec(dep) ?? /\/r\/([a-z0-9-]+)\.json$/.exec(dep)
      if (!m) fail(`${slug} depends on ${dep}, which is not an affectUI item`)
      queue.push(m[1])
    }
  }
  const sorted = Object.fromEntries(Object.entries(items).sort(([a], [b]) => a.localeCompare(b)))
  const next = { roots, items: sorted }
  checkLock(next)
  writeAtomic(LOCK, JSON.stringify(next, null, 2) + '\n')
  writePro(next)
  checkDependencies(next)
  for (const [slug, i] of Object.entries(sorted)) console.log(`affectui: ${i.tier.padEnd(4)} ${slug}`)
}

/**
 * Nothing tracked by git may be a Pro file: not by its name anywhere in the app, and not by its
 * content under any name (a Pro file copied into `components/fx/` and renamed is still Pro).
 */
function check() {
  const lock = readLock()
  checkLock(lock)
  const proNames = new Set(proFiles(lock))
  const proHashes = new Set(
    Object.values(lock.items)
      .filter((i) => i.tier === 'pro')
      .flatMap((i) => Object.values(i.files)),
  )
  let tracked
  try {
    tracked = execFileSync('git', ['ls-files', '--', '.'], { cwd: ROOT, encoding: 'utf8' })
  } catch (e) {
    fail(`git ls-files failed: ${e.message}`)
  }
  const leaked = tracked
    .split('\n')
    .map((p) => p.trim())
    .filter(Boolean)
    .filter((p) => {
      if (proNames.has(basename(p))) return true
      if (!/\.(tsx|ts|css)$/.test(p)) return false
      const text = read(join(ROOT, p))
      return text !== undefined && proHashes.has(sha256(text))
    })
  if (leaked.length) fail(`affectUI Pro source is tracked by git: ${leaked.join(', ')}. Remove it with git rm --cached.`)
  console.log(`affectui: ${proNames.size} Pro files, none tracked`)
}

const args = process.argv.slice(2)
if (args[0] === '--update') await update(args.slice(1))
else if (args[0] === '--check') check()
else if (args.length === 0) await ensure()
else fail(`unknown arguments: ${args.join(' ')}`)
