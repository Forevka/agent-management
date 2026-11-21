//! The kit: the owner's Claude Code skills, plugins and MCP servers, kept by the orchestrator
//! and installed on every node.
//!
//! - [`KitCaptureRequest`] asks a node what it has (the import); it answers with
//!   [`KitCapture`] parts.
//! - [`Kit`] is the whole enabled kit, the desired state: a node installs what is in it and
//!   removes what it installed earlier and is no longer in it. It answers with
//!   [`KitItemStatus`]es.
//!
//! A kit can be larger than one frame ([`crate::MAX_FRAME_LEN`]): a skill may hold up to
//! [`MAX_SKILL_BYTES`] of files. So both directions travel in parts of at most
//! [`PART_BUDGET`] bytes each, every part a self-contained list of whole items. A receiver
//! collects the parts of one id from part 0 to the part marked `last`; a part 0 starts over, so
//! a sequence cut short (the sender restarted, losing its outbox) is replaced by the next one.

use serde::{Deserialize, Serialize};

/// Upper bound on the files of one skill together, in bytes. Skills are instructions, not
/// assets: anything larger is not a skill the kit should copy to every machine.
pub const MAX_SKILL_BYTES: usize = 2 * 1024 * 1024;
/// Upper bound on one file of a skill.
pub const MAX_SKILL_FILE_BYTES: usize = 1024 * 1024;
/// Upper bound on the files of one skill, so a skill that carries a dependency tree is refused
/// instead of copied file by file.
pub const MAX_SKILL_FILES: usize = 200;
/// How much of a frame one part may fill. One skill fits with room to spare (it is at most
/// [`MAX_SKILL_BYTES`]), and the rest of [`crate::MAX_FRAME_LEN`] covers the encoding overhead.
pub const PART_BUDGET: usize = 3 * 1024 * 1024;

/// Orchestrator -> node: report the skills, plugins and MCP servers this machine has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KitCaptureRequest {
    /// Echoed in every part of the answer.
    pub id: String,
}

/// Node -> orchestrator, only in answer to a [`KitCaptureRequest`]: one part of what this
/// machine's Claude Code has at user scope.
///
/// MCP server configs travel as they are, secrets included, over the authenticated link: the
/// orchestrator moves every env and header value into its vault at once and keeps only
/// placeholders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KitCapture {
    pub id: String,
    /// 0, 1, ... in order.
    pub part: u32,
    /// The final part.
    pub last: bool,
    pub content: KitContent,
    /// What the node saw and left out, and why (too large, binary, unreadable).
    pub skipped: Vec<KitSkipped>,
}

/// Orchestrator -> node: one part of the full enabled kit, with every MCP secret resolved for
/// the node. Sent on connect and after every change, only to a node that is current
/// ([`crate::node_is_current`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kit {
    /// Identifies this sending of the kit; every part carries it.
    pub revision: String,
    pub part: u32,
    pub last: bool,
    pub content: KitContent,
}

/// Node -> orchestrator, only in answer to a [`Kit`]: how one item fared on this node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KitItemStatus {
    /// `skill`, `plugin` or `mcp`.
    pub kind: String,
    pub name: String,
    /// `installed`, `failed`, `conflict` (the machine has its own item of that name, which the
    /// kit leaves alone) or `removed` (taken off the machine because the kit no longer has it).
    pub state: String,
    /// Why, for the owner to read. Never holds a secret value.
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KitContent {
    pub skills: Vec<KitSkill>,
    pub plugins: Vec<KitPlugin>,
    pub mcp: Vec<KitMcp>,
}

/// A skill: the directory `<claude config>/skills/<name>/` and its text files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KitSkill {
    pub name: String,
    pub files: Vec<KitFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KitFile {
    /// Relative to the skill's directory, with `/` separators ([`safe_skill_path`]).
    pub path: String,
    pub text: String,
}

/// A plugin, installed from its marketplace with the `claude plugin` CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KitPlugin {
    pub plugin: String,
    pub marketplace: String,
    /// What `claude plugin marketplace add` takes: `owner/repo`, a git URL, or a path.
    pub marketplace_source: String,
}

impl KitPlugin {
    /// `plugin@marketplace`, the id Claude Code knows it by and the kit item's name.
    pub fn id(&self) -> String {
        format!("{}@{}", self.plugin, self.marketplace)
    }
}

/// A user-scope MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KitMcp {
    pub name: String,
    /// The server's JSON config (`command`/`args`/`env`, or `type`/`url`/`headers`).
    pub config: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KitSkipped {
    pub kind: String,
    pub name: String,
    pub reason: String,
}

impl KitSkill {
    /// About what this skill takes on the wire.
    pub fn size(&self) -> usize {
        self.name.len()
            + self
                .files
                .iter()
                .map(|f| f.path.len() + f.text.len() + 16)
                .sum::<usize>()
    }
}

impl KitContent {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.plugins.is_empty() && self.mcp.is_empty()
    }

    /// Splits into parts of at most `budget` bytes each (by [`KitSkill::size`] and the lengths
    /// of the other items), keeping every item whole. Always at least one part, which may be
    /// empty: an empty kit is still a desired state ("nothing").
    pub fn split(self, budget: usize) -> Vec<KitContent> {
        let mut parts = vec![KitContent::default()];
        let mut used = 0usize;
        let mut room = |size: usize, parts: &mut Vec<KitContent>| {
            if used > 0 && used + size > budget {
                parts.push(KitContent::default());
                used = 0;
            }
            used += size;
        };
        for s in self.skills {
            room(s.size(), &mut parts);
            parts.last_mut().unwrap().skills.push(s);
        }
        for p in self.plugins {
            room(
                p.plugin.len() + p.marketplace.len() + p.marketplace_source.len() + 16,
                &mut parts,
            );
            parts.last_mut().unwrap().plugins.push(p);
        }
        for m in self.mcp {
            room(m.name.len() + m.config.len() + 16, &mut parts);
            parts.last_mut().unwrap().mcp.push(m);
        }
        parts
    }

    /// Appends another part's items.
    pub fn extend(&mut self, other: KitContent) {
        self.skills.extend(other.skills);
        self.plugins.extend(other.plugins);
        self.mcp.extend(other.mcp);
    }
}

/// Collects the parts of one sequence ([`Kit`] or [`KitCapture`]) in order.
#[derive(Debug, Default)]
pub struct Parts {
    id: Option<String>,
    next: u32,
    content: KitContent,
}

impl Parts {
    /// Takes one part. Returns the whole content once the `last` part of an unbroken sequence
    /// is in. A part 0 starts a new sequence. A part of another id (a stale sequence the sender
    /// gave up on) is ignored, and the sequence in progress goes on. A gap within the current
    /// sequence (the sender restarted mid-way) drops what was collected; the sender's next
    /// sequence replaces it.
    pub fn add(
        &mut self,
        id: &str,
        part: u32,
        last: bool,
        content: KitContent,
    ) -> Option<KitContent> {
        if part == 0 {
            self.id = Some(id.to_owned());
            self.next = 0;
            self.content = KitContent::default();
        } else if self.id.as_deref() != Some(id) {
            return None;
        } else if part != self.next {
            self.id = None;
            self.content = KitContent::default();
            return None;
        }
        self.content.extend(content);
        self.next += 1;
        if last {
            self.id = None;
            return Some(std::mem::take(&mut self.content));
        }
        None
    }
}

/// Whether a message fits one frame. Kit messages are checked before they are queued: a frame
/// the link cannot send would sit in the outbox for ever, and everything after it with it.
pub fn fits(body: &crate::Body) -> bool {
    crate::Envelope {
        seq: u64::MAX,
        ack: u64::MAX,
        body: body.clone(),
    }
    .encode()
    .is_ok()
}

/// Whether `name` can be a skill, plugin, marketplace or MCP server name in the kit: what the
/// node turns into a directory name or passes to the CLI must not be able to point anywhere
/// else. Letters, digits, `-`, `_`, `.` (not first or last), at most 100 characters, and not a
/// name Windows opens as a device.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !windows_device(name)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether `path` is a file path the kit may write inside a skill's directory: relative, `/`
/// separated, no `..`, `.` or empty component, nothing a Windows path would read as a drive or
/// a device stream, and not the kit's own marker.
pub fn safe_skill_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 255
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && !path.contains('\0')
        && path != SKILL_MARKER
        && path.split('/').all(|c| {
            !c.is_empty()
                && c != "."
                && c != ".."
                && c.trim() == c
                // Windows drops a trailing dot, and opens a device for a reserved name: two
                // different names would land on one file, or on none.
                && !c.ends_with('.')
                && !windows_device(c)
        })
}

/// `CON`, `nul.txt`, `COM1`: names Windows opens as a device whatever the extension.
fn windows_device(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
}

/// Whether a skill's file is, by its name, where a secret lives (`.env`, a key, credentials, a
/// package registry's login). Such a file is never taken into the kit: its content would sit in
/// the database and go to every machine, outside the vault.
pub fn secret_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".env")
        || name.contains("secret")
        || name.contains("token")
        || name.starts_with("credentials")
        || name.starts_with("service-account")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || matches!(name.as_str(), ".netrc" | "_netrc" | ".npmrc" | ".pypirc")
        || [".pem", ".key", ".p12", ".pfx", ".ppk", ".keystore", ".jks"]
            .iter()
            .any(|ext| name.ends_with(ext))
}

/// Whether a variable, flag, header or key name says its value is a secret (`--api-key`, `token`,
/// `DB_PASSWORD`, `Authorization`, `clientSecret`).
pub fn sensitive_name(name: &str) -> bool {
    let n = name.trim_start_matches('-').to_ascii_lowercase();
    [
        "token",
        "key",
        "secret",
        "password",
        "passwd",
        "pwd",
        "auth",
        "credential",
        "bearer",
        "cookie",
        "session",
        "dsn",
    ]
    .iter()
    .any(|w| n.contains(w))
}

// ---------------------------------------------------------------- MCP configs: never shown raw
//
// An MCP server's config is a secret as a whole: the orchestrator keeps it sealed in its vault and
// Postgres sees only `config_summary`. What follows is for *showing* a config to the owner
// (`mask_config`) and for scrubbing transcripts (`scrub_values`). Masking is a heuristic over the
// shapes secrets take, backed by a guard for anything that merely looks like a token; nothing
// stored depends on it catching everything.

/// What stands in a masked config where a secret was.
pub const MASK: &str = "[masked]";

/// Keys under which every string is a secret, whatever its own name.
const SECRET_SECTIONS: [&str; 3] = ["env", "headers", "oauth"];

/// Flags whose next argument is a secret, besides those named like one (`-p pass`).
const SECRET_NEXT: [&str; 2] = ["-p", "-P"];

/// Flags whose next argument is an HTTP header (`--header "Authorization: Bearer …"`).
const HEADER_FLAGS: [&str; 2] = ["-H", "--header"];

/// A copy of an MCP server's config with every secret it can find replaced by [`MASK`], and the
/// secrets themselves (for scrubbing).
pub fn mask_config(config: &serde_json::Value) -> (serde_json::Value, Vec<String>) {
    let mut found = Vec::new();
    let masked = mask_value(config, false, &mut found);
    found.retain(|s| !s.is_empty());
    found.sort();
    found.dedup();
    (masked, found)
}

fn mask_value(v: &serde_json::Value, secret: bool, found: &mut Vec<String>) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::String(s) if secret => {
            found.push(s.clone());
            Value::String(MASK.into())
        }
        Value::String(s) => Value::String(mask_string(s, found)),
        Value::Array(a) if !secret => Value::Array(mask_args(a, found)),
        Value::Array(a) => Value::Array(a.iter().map(|x| mask_value(x, true, found)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    let inner =
                        secret || SECRET_SECTIONS.contains(&k.as_str()) || sensitive_name(k);
                    (k.clone(), mask_value(x, inner, found))
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A list of arguments, where a flag decides what its next one is.
fn mask_args(args: &[serde_json::Value], found: &mut Vec<String>) -> Vec<serde_json::Value> {
    use serde_json::Value;
    enum Next {
        Plain,
        Secret,
        Header,
    }
    let mut next = Next::Plain;
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        let Value::String(s) = a else {
            out.push(mask_value(a, false, found));
            next = Next::Plain;
            continue;
        };
        let masked = match std::mem::replace(&mut next, Next::Plain) {
            Next::Secret => {
                found.push(s.clone());
                MASK.to_owned()
            }
            Next::Header => mask_header(s, true, found).unwrap_or_else(|| mask_string(s, found)),
            Next::Plain if s.starts_with('-') => match s.split_once('=') {
                Some((flag, v)) if HEADER_FLAGS.contains(&flag) => {
                    let v = mask_header(v, true, found).unwrap_or_else(|| mask_string(v, found));
                    format!("{flag}={v}")
                }
                Some((flag, v)) if sensitive_name(flag) && !v.is_empty() => {
                    found.push(v.to_owned());
                    format!("{flag}={MASK}")
                }
                Some((flag, v)) => format!("{flag}={}", mask_string(v, found)),
                None => {
                    if HEADER_FLAGS.contains(&s.as_str()) {
                        next = Next::Header;
                    } else if sensitive_name(s) || SECRET_NEXT.contains(&s.as_str()) {
                        next = Next::Secret;
                    }
                    mask_string(s, found)
                }
            },
            Next::Plain => mask_string(s, found),
        };
        // A flag's "next" never swallows another flag.
        if matches!(next, Next::Secret)
            && let Some(Value::String(n)) = args.get(out.len() + 1)
            && n.starts_with('-')
        {
            next = Next::Plain;
        }
        out.push(Value::String(masked));
    }
    out
}

/// `Name: value` as an HTTP header, with the value masked when the name says it is a secret, or
/// always when `any` (it came after `--header`). `None` when `s` is not a header.
fn mask_header(s: &str, any: bool, found: &mut Vec<String>) -> Option<String> {
    let (name, value) = s.split_once(':')?;
    let header_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !header_name || value.starts_with("//") {
        return None;
    }
    let value = value.trim();
    if value.is_empty() || !(any || sensitive_name(name)) {
        return None;
    }
    found.push(value.to_owned());
    // The scheme alone (`Bearer`) is no secret, and keeps the masked view readable.
    if let Some((scheme, token)) = value.split_once(' ')
        && matches!(
            scheme.to_ascii_lowercase().as_str(),
            "bearer" | "basic" | "token"
        )
    {
        found.push(token.trim().to_owned());
        return Some(format!("{name}: {scheme} {MASK}"));
    }
    Some(format!("{name}: {MASK}"))
}

/// One string anywhere in a config: JSON inside it, a header, then each whitespace-separated
/// token by the rules for URLs, DSNs, `key=value` pairs and anything that looks like a token.
fn mask_string(s: &str, found: &mut Vec<String>) -> String {
    let t = s.trim();
    if (t.starts_with('{') || t.starts_with('['))
        && let Ok(inner) = serde_json::from_str::<serde_json::Value>(t)
    {
        return mask_value(&inner, false, found).to_string();
    }
    if let Some(h) = mask_header(s, false, found) {
        return h;
    }
    let mut out = Vec::new();
    let mut after_scheme = false;
    for token in s.split(' ') {
        if after_scheme && !token.is_empty() {
            found.push(token.to_owned());
            out.push(MASK.to_owned());
            after_scheme = false;
            continue;
        }
        after_scheme = matches!(token.to_ascii_lowercase().as_str(), "bearer" | "basic");
        out.push(mask_token(token, found));
    }
    out.join(" ")
}

/// One token: `key=value`, a URL (userinfo, every query value), a DSN without a scheme
/// (`user:pass@host`), or a token by its look.
fn mask_token(token: &str, found: &mut Vec<String>) -> String {
    if token.is_empty() {
        return String::new();
    }
    // `key=value`, as libpq, env files and `-e` take them. A URL's own `=` is the URL's.
    if !token.contains("://")
        && let Some((k, v)) = token.split_once('=')
        && !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        if sensitive_name(k) && !v.is_empty() {
            found.push(v.to_owned());
            return format!("{k}={MASK}");
        }
        return format!("{k}={}", mask_token(v, found));
    }
    if let Some(i) = token.find("://") {
        return mask_url(token, i, found);
    }
    // `user:pass@host:5432/db`: a password before an `@`.
    if let Some(at) = token.find('@')
        && let Some(colon) = token[..at].find(':')
        && colon + 1 < at
    {
        let pass = &token[colon + 1..at];
        found.push(pass.to_owned());
        return format!("{}{MASK}{}", &token[..colon + 1], &token[at..]);
    }
    if looks_like_token(token) {
        found.push(token.to_owned());
        return MASK.to_owned();
    }
    token.to_owned()
}

/// A URL with its userinfo masked (a user alone too, which is how tokens ride in git URLs; only
/// ssh's plain `git@` is kept) and every query value masked.
fn mask_url(url: &str, scheme_end: usize, found: &mut Vec<String>) -> String {
    let start = scheme_end + 3;
    let rest = &url[start..];
    let auth_end = rest.find(['/', '?', '#']).map_or(rest.len(), |i| i);
    let authority = &rest[..auth_end];
    let mut out = url[..start].to_owned();
    match authority.rfind('@') {
        Some(at) if &authority[..at] != "git" => {
            let info = &authority[..at];
            found.push(info.to_owned());
            // The password alone too: a transcript may show it without its user.
            if let Some((_, pass)) = info.split_once(':') {
                found.push(pass.to_owned());
            }
            out.push_str(MASK);
            out.push_str(&authority[at..]);
        }
        _ => out.push_str(authority),
    }
    let tail = &rest[auth_end..];
    let (before_frag, frag) = match tail.find('#') {
        Some(i) => (&tail[..i], &tail[i..]),
        None => (tail, ""),
    };
    match before_frag.split_once('?') {
        Some((path, query)) => {
            out.push_str(path);
            out.push('?');
            let pairs: Vec<String> = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((k, v)) if !v.is_empty() => {
                        found.push(v.to_owned());
                        format!("{k}={MASK}")
                    }
                    _ => pair.to_owned(),
                })
                .collect();
            out.push_str(&pairs.join("&"));
        }
        None => out.push_str(before_frag),
    }
    out.push_str(frag);
    out
}

/// Whether a token looks like a credential by itself: a well-known prefix, an AWS key id, a JWT,
/// or a long high-entropy string.
pub fn looks_like_token(token: &str) -> bool {
    let t = token.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | '(' | ')'));
    const PREFIXES: [&str; 17] = [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xoxa-",
        "xoxb-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "sk-",
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "AIza",
    ];
    if PREFIXES
        .iter()
        .any(|p| t.starts_with(p) && t.len() >= p.len() + 8)
    {
        return true;
    }
    if t.len() == 20
        && t.starts_with("AKIA")
        && t[4..]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return true;
    }
    if t.starts_with("eyJ") && t.matches('.').count() == 2 && t.contains(".eyJ") {
        return true;
    }
    let charset = t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '='));
    let digit = t.chars().any(|c| c.is_ascii_digit());
    let lower = t.chars().any(|c| c.is_ascii_lowercase());
    let upper = t.chars().any(|c| c.is_ascii_uppercase());
    charset
        && !t.starts_with('-')
        && ((t.len() >= 32 && digit && lower && upper)
            || (t.len() >= 40 && digit && (lower || upper)))
}

/// What may be said about an MCP server's config in Postgres, the API and the logs: its type,
/// its command's name (no path, no arguments) or its endpoint's scheme and host (no userinfo,
/// path or query). Nothing else of the config leaves the vault in the clear.
pub fn config_summary(config: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    let mut out = Map::new();
    let url = config.get("url").and_then(Value::as_str);
    let command = config.get("command").and_then(Value::as_str);
    let kind = match config.get("type").and_then(Value::as_str) {
        Some(t @ ("stdio" | "http" | "sse" | "ws" | "sse-ide" | "ws-ide")) => t.to_owned(),
        _ if url.is_some() => "http".into(),
        _ => "stdio".into(),
    };
    out.insert("type".into(), Value::String(kind));
    if let Some(c) = command {
        let name = c.rsplit(['/', '\\']).next().unwrap_or(c);
        let plain = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
            && !looks_like_token(name);
        out.insert(
            "command".into(),
            Value::String(if plain {
                name.into()
            } else {
                "(not shown)".into()
            }),
        );
    }
    if let Some(u) = url
        && let Some(i) = u.find("://")
    {
        let scheme = &u[..i];
        let rest = &u[i + 3..];
        let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = host.split(':').next().unwrap_or(host);
        let plain = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
        };
        if plain(scheme) && plain(host) {
            out.insert(
                "endpoint".into(),
                Value::String(format!("{scheme}://{host}")),
            );
        }
    }
    Value::Object(out)
}

/// The strings to scrub from a transcript for a resolved MCP config: whatever [`mask_config`]
/// finds, and every string in it of 8 characters or more (and each such word of a longer one)
/// that is not a flag, a plain word, or the server's `type` or `command` — so a secret in a shape
/// no rule knows is still scrubbed.
pub fn scrub_values(config: &serde_json::Value) -> Vec<String> {
    use serde_json::Value;
    let (_, mut out) = mask_config(config);
    fn plain(s: &str) -> bool {
        let flag = s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        flag || s.chars().all(|c| c.is_ascii_alphabetic())
    }
    fn walk(v: &Value, key: Option<&str>, out: &mut Vec<String>) {
        match v {
            Value::String(s) if !matches!(key, Some("type" | "command")) => {
                for part in std::iter::once(s.as_str()).chain(s.split_whitespace()) {
                    if part.len() >= 8 && !plain(part) {
                        out.push(part.to_owned());
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, key, out)),
            Value::Object(m) => m.iter().for_each(|(k, x)| walk(x, Some(k), out)),
            _ => {}
        }
    }
    walk(config, None, &mut out);
    out.sort();
    out.dedup();
    out
}

/// Whether a plugin marketplace source may be kept: `owner/repo`, a URL or a path with no
/// userinfo of any kind (a token rides as a user too), no query or fragment, and nothing a CLI
/// would read as a flag. An scp-style `git@host:owner/repo` is the one `@` allowed.
pub fn clean_source(source: &str) -> bool {
    let s = source.trim();
    if s.is_empty()
        || s.starts_with('-')
        || s.contains(char::is_whitespace)
        || s.contains(['?', '#'])
    {
        return false;
    }
    match s.find("://") {
        Some(i) => {
            let rest = &s[i + 3..];
            let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
            !authority.contains('@')
        }
        None => match s.split_once('@') {
            Some((user, _)) => user == "git" && !s[4..].contains('@'),
            None => true,
        },
    }
}

/// The file a node writes into every skill directory the kit installed, holding the content
/// hash: it is how the node tells its own work from the owner's.
pub const SKILL_MARKER: &str = ".fleet-kit";

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, bytes: usize) -> KitSkill {
        KitSkill {
            name: name.into(),
            files: vec![KitFile {
                path: "SKILL.md".into(),
                text: "x".repeat(bytes),
            }],
        }
    }

    #[test]
    fn a_large_kit_travels_in_parts_that_each_fit_a_frame() {
        let content = KitContent {
            skills: (0..5)
                .map(|i| skill(&format!("s{i}"), MAX_SKILL_BYTES))
                .collect(),
            plugins: vec![KitPlugin {
                plugin: "p".into(),
                marketplace: "m".into(),
                marketplace_source: "o/r".into(),
            }],
            mcp: vec![KitMcp {
                name: "x".into(),
                config: "{}".into(),
            }],
        };
        let parts = content.clone().split(PART_BUDGET);
        assert_eq!(parts.len(), 5, "one big skill per part");
        let n = parts.len();
        let mut rx = Parts::default();
        let mut whole = None;
        for (i, p) in parts.into_iter().enumerate() {
            let env = crate::Envelope {
                seq: 1,
                ack: 0,
                body: crate::Body::Kit(Kit {
                    revision: "r1".into(),
                    part: i as u32,
                    last: i + 1 == n,
                    content: p,
                }),
            };
            let bytes = env.encode().expect("every part fits a frame");
            let crate::Body::Kit(k) = crate::Envelope::decode(&bytes).unwrap().body else {
                panic!()
            };
            whole = rx.add(&k.revision, k.part, k.last, k.content);
        }
        assert_eq!(whole, Some(content));
        // Nothing at all is still one part, so "the kit is empty" can be said.
        assert_eq!(KitContent::default().split(PART_BUDGET).len(), 1);
    }

    #[test]
    fn a_broken_sequence_is_dropped_and_the_next_one_wins() {
        let mut rx = Parts::default();
        let c = |n: &str| KitContent {
            mcp: vec![KitMcp {
                name: n.into(),
                config: "{}".into(),
            }],
            ..Default::default()
        };
        assert_eq!(rx.add("a", 0, false, c("a0")), None);
        // Part 1 of "a" never came: the sender restarted and began "b".
        assert_eq!(rx.add("b", 0, false, c("b0")), None);
        assert_eq!(
            rx.add("a", 1, true, c("a1")),
            None,
            "a stale part is ignored"
        );
        let mut both = c("b0");
        both.extend(c("b1"));
        assert_eq!(
            rx.add("b", 1, true, c("b1")),
            Some(both),
            "and the sequence in progress goes on"
        );
        // A gap within the current sequence drops it; the next sequence starts afresh.
        assert_eq!(rx.add("d", 0, false, c("d0")), None);
        assert_eq!(
            rx.add("d", 2, true, c("d2")),
            None,
            "a gap drops the sequence"
        );
        assert_eq!(
            rx.add("d", 1, true, c("d1")),
            None,
            "nothing is left to finish"
        );
        assert_eq!(rx.add("e", 0, true, c("e0")), Some(c("e0")));
    }

    #[test]
    fn names_and_paths_cannot_point_outside_their_place() {
        for ok in ["fleet", "my-skill", "a_b.c", "pdf", "console", "com10"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["NUL", "con", "aux.md", "COM1", "lpt9", "trailing."] {
            assert!(!valid_name(bad), "{bad}");
        }
        for bad in [
            "",
            ".hidden",
            "../x",
            "a/b",
            "a\\b",
            "c:",
            "a b",
            &"x".repeat(101),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        for ok in ["SKILL.md", "scripts/run.py", "ref/a b.md"] {
            assert!(safe_skill_path(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/./b",
            "a//b",
            "C:/x",
            "a\\b",
            "x:stream",
            ".fleet-kit",
            " a",
            "NUL",
            "ref/con.md",
            "com1.txt",
            "trailing.",
        ] {
            assert!(!safe_skill_path(bad), "{bad:?}");
        }
        assert!(safe_skill_path("console.md") && safe_skill_path("com10.md"));
        for secret in [
            ".env",
            ".env.local",
            "prod.env",
            "cert.pem",
            "ref/server.key",
            "credentials.json",
            "id_rsa",
            "my-secrets.md",
            "api_token.txt",
            ".netrc",
            ".npmrc",
            ".pypirc",
            "putty.ppk",
            "service-account-prod.json",
        ] {
            assert!(secret_file(secret), "{secret}");
        }
        for plain in ["SKILL.md", "envelope.md", "keys.md", "ref/api.md"] {
            assert!(!secret_file(plain), "{plain}");
        }
    }

    /// Every shape a secret was found in by review, and more: none survives in what Postgres, the
    /// API or the audit could see (the summary, the masked view), and every one is scrubbed from a
    /// transcript.
    #[test]
    fn no_secret_survives_the_summary_the_masked_view_or_a_transcript() {
        use serde_json::json;
        let shapes: Vec<(&str, serde_json::Value)> = vec![
            (
                "RAWENV001",
                json!({"command": "x", "env": {"API_KEY": "RAWENV001"}}),
            ),
            (
                "RAWENV002",
                json!({"command": "x", "env": {"MODE": "RAWENV002"}}),
            ),
            (
                "RAWNEST03",
                json!({"command": "x", "env": {"A": {"b": "RAWNEST03"}}}),
            ),
            (
                "RAWOAUTH4",
                json!({"type": "http", "url": "https://h/x", "oauth": {"clientSecret": "RAWOAUTH4"}}),
            ),
            (
                "RAWHDR005",
                json!({"type": "http", "url": "https://h/x", "headers": {"X-Team": "RAWHDR005"}}),
            ),
            (
                "RAWARG006",
                json!({"command": "mcp-remote", "args": ["https://h/x", "--header", "Authorization: Bearer RAWARG006"]}),
            ),
            (
                "RAWARG007",
                json!({"command": "mcp-remote", "args": ["--header=Authorization: Bearer RAWARG007"]}),
            ),
            (
                "RAWARG008",
                json!({"command": "x", "args": ["Authorization: Bearer RAWARG008"]}),
            ),
            (
                "RAWARG009",
                json!({"command": "x", "args": ["-H", "X-Api-Key: RAWARG009"]}),
            ),
            (
                "ghp_RAWUSER10abc",
                json!({"type": "http", "url": "https://ghp_RAWUSER10abc@github.com/o/r"}),
            ),
            (
                "ghp_RAWUSER11abc",
                json!({"command": "git", "args": ["clone", "https://ghp_RAWUSER11abc@github.com/o/r"]}),
            ),
            (
                "RAWPASS12",
                json!({"type": "http", "url": "https://bot:RAWPASS12@h/x"}),
            ),
            (
                "RAWQRY013",
                json!({"type": "http", "url": "https://h/x?team=a&key=RAWQRY013#f"}),
            ),
            (
                "RAWQRY014",
                json!({"command": "x", "args": ["--url", "https://h/x?access=RAWQRY014"]}),
            ),
            (
                "RAWDSN015",
                json!({"command": "x", "args": ["postgresql://u:RAWDSN015@db:5432/app"]}),
            ),
            (
                "RAWDSN016",
                json!({"command": "x", "args": ["u:RAWDSN016@db:5432/app"]}),
            ),
            (
                "RAWPQ0017",
                json!({"command": "x", "args": ["host=h user=u password=RAWPQ0017 dbname=d"]}),
            ),
            ("RAWCMD018", json!({"command": "TOKEN=RAWCMD018 npx"})),
            ("RAWCMD019", json!({"command": "https://u:RAWCMD019@h/run"})),
            (
                "RAWP00020",
                json!({"command": "mysql-mcp", "args": ["-u", "root", "-p", "RAWP00020"]}),
            ),
            (
                "RAWJSON21",
                json!({"command": "x", "args": ["--config", "{\"apiKey\": \"RAWJSON21\"}"]}),
            ),
            (
                "RAWJSON22",
                json!({"command": "x", "args": ["{\"db\": {\"password\": \"RAWJSON22\"}}"]}),
            ),
            (
                "RAWDOCK23",
                json!({"command": "docker", "args": ["run", "-e", "GITHUB_TOKEN=RAWDOCK23", "img"]}),
            ),
            (
                "RAWDOCK24",
                json!({"command": "docker", "args": ["--env=SLACK_TOKEN=RAWDOCK24"]}),
            ),
            (
                "RAWFLAG25",
                json!({"command": "x", "args": ["--api-key", "RAWFLAG25"]}),
            ),
            (
                "RAWFLAG26",
                json!({"command": "x", "args": ["--token=RAWFLAG26"]}),
            ),
            (
                "RAWFLAG27",
                json!({"command": "x", "args": ["--client-secret", "RAWFLAG27"]}),
            ),
            (
                "xoxb-RAWSLACK28-1234",
                json!({"command": "x", "args": ["xoxb-RAWSLACK28-1234"]}),
            ),
            (
                "sk-RAWOPENAI29abcdef",
                json!({"command": "x", "args": ["--model", "gpt", "sk-RAWOPENAI29abcdef"]}),
            ),
            (
                "AKIARAWAWS30EXAMPLE1",
                json!({"command": "x", "args": ["AKIARAWAWS30EXAMPLE1"]}),
            ),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJSQVczMSJ9.c2lnbmF0dXJl",
                json!({"command": "x", "args": ["eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJSQVczMSJ9.c2lnbmF0dXJl"]}),
            ),
            (
                "Q9x7RawEntropy32TokenZ8kLmN4pQrS",
                json!({"command": "x", "args": ["Q9x7RawEntropy32TokenZ8kLmN4pQrS"]}),
            ),
            (
                "RAWDBURL33",
                json!({"command": "x", "args": ["DATABASE_URL=postgres://u:RAWDBURL33@h/d"]}),
            ),
            (
                "RAWBASIC34",
                json!({"command": "x", "args": ["--auth", "Basic RAWBASIC34"]}),
            ),
        ];
        for (raw, config) in shapes {
            let (masked, found) = mask_config(&config);
            let summary = config_summary(&config);
            for (what, text) in [
                ("masked view", masked.to_string()),
                ("summary", summary.to_string()),
            ] {
                assert!(
                    !text.contains(raw),
                    "{raw} survives in the {what}: {text} (from {config})"
                );
            }
            assert!(
                found.iter().any(|f| f.contains(raw)),
                "{raw} not found in {config}: {found:?}"
            );
            let mut transcript = format!("tool output: {raw} and the config {config}");
            crate::redact::redact_secrets(&mut transcript, &scrub_values(&config));
            assert!(
                !transcript.contains(raw),
                "{raw} not scrubbed: {transcript}"
            );
        }

        // A config stays readable where it holds no secret.
        let plain = json!({"type": "stdio", "command": "/usr/bin/npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "--port", "8080"], "env": {}});
        assert_eq!(mask_config(&plain).0, plain);
        assert_eq!(
            config_summary(&plain),
            json!({"type": "stdio", "command": "npx"})
        );
        assert_eq!(
            config_summary(
                &json!({"type": "http", "url": "https://bot:pw@mcp.example.com:8443/x?k=v"})
            ),
            json!({"type": "http", "endpoint": "https://mcp.example.com"})
        );
        assert_eq!(
            mask_config(&json!({"args": ["ssh://git@github.com/o/r"]})).0,
            json!({"args": ["ssh://git@github.com/o/r"]}),
            "ssh's plain git@ is no secret"
        );
    }

    #[test]
    fn a_marketplace_source_never_carries_a_credential() {
        for ok in [
            "o/r",
            "https://github.com/o/r.git",
            "git@github.com:o/r.git",
            "C:/m",
            "./m",
        ] {
            assert!(clean_source(ok), "{ok}");
        }
        for bad in [
            "https://ghp_x@github.com/o/r",
            "https://u:p@h/r",
            "https://h/r?token=x",
            "https://h/r#x",
            "bot@github.com:o/r",
            "--scope=project",
            "a b",
            "",
        ] {
            assert!(!clean_source(bad), "{bad}");
        }
    }
}
