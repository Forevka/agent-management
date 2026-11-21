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

/// Whether a variable, flag or parameter name says its value is a secret (`--api-key`, `token`,
/// `DB_PASSWORD`).
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

/// The `user:password` of a `scheme://user:password@host` in `s`, if it has one with a password.
fn userinfo(s: &str) -> Option<&str> {
    let start = s.find("://")? + 3;
    let rest = &s[start..];
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let info = &authority[..authority.rfind('@')?];
    (info.contains(':') && !info.contains("${secret:")).then_some(info)
}

/// Whether `s` carries a password in a `scheme://user:password@host`.
pub fn has_credentials(s: &str) -> bool {
    userinfo(s).is_some()
}

/// `NAME=value` where `NAME` reads as a variable name and says it is a secret.
fn named_secret(s: &str) -> Option<(&str, &str)> {
    let (name, value) = s.split_once('=')?;
    let is_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    (is_name && sensitive_name(name) && !value.is_empty()).then_some((name, value))
}

/// Where an MCP server's config holds a secret:
///
/// - every `env` and `headers` value;
/// - in `url`: a `user:password@`, and every query value;
/// - in `args`: a `user:password@` in any argument (a database DSN), `NAME=value` with a secret
///   name (also as docker's `-e NAME=value` or `--env=NAME=value`), and a flag named like a secret
///   with its value (`--token=…`, `--api-key …`).
///
/// Both sides use it: the orchestrator to move each one into the vault, the node to scrub the
/// resolved values from transcripts.
pub fn secret_spots(config: &serde_json::Value) -> Vec<SecretSpot> {
    use serde_json::Value;
    let mut out = Vec::new();
    let spot = |at: SpotAt, name: &str, prefix: &str, value: &str, suffix: &str| SecretSpot {
        at,
        name: name.to_owned(),
        prefix: prefix.to_owned(),
        value: value.to_owned(),
        suffix: suffix.to_owned(),
    };
    for (section, kind) in [("env", SpotAt::Env), ("headers", SpotAt::Header)] {
        if let Some(Value::Object(m)) = config.get(section) {
            for (k, v) in m {
                if let Value::String(s) = v {
                    out.push(spot(kind.clone(), k, "", s, ""));
                }
            }
        }
    }
    if let Some(Value::String(url)) = config.get("url") {
        if let Some(info) = userinfo(url) {
            out.push(spot(SpotAt::Url, "url_userinfo", "//", info, "@"));
        }
        if let Some((_, query)) = url.split_once('?') {
            let query = query.split('#').next().unwrap_or(query);
            for pair in query.split('&') {
                if let Some((k, v)) = pair.split_once('=')
                    && !v.is_empty()
                {
                    out.push(spot(SpotAt::Url, k, &format!("{k}="), v, ""));
                }
            }
        }
    }
    if let Some(Value::Array(args)) = config.get("args") {
        let mut taken_next = false;
        for (i, a) in args.iter().enumerate() {
            let Some(a) = a.as_str() else { continue };
            if std::mem::take(&mut taken_next) {
                continue;
            }
            let before = out.len();
            if a.starts_with('-') {
                let flag = a;
                match flag.split_once('=') {
                    Some((f, v)) if sensitive_name(f) && !v.is_empty() => {
                        out.push(spot(
                            SpotAt::Arg(i),
                            f.trim_start_matches('-'),
                            &format!("{f}="),
                            v,
                            "",
                        ));
                    }
                    // `--env=NAME=value`
                    Some((f, v)) => {
                        if let Some((n, value)) = named_secret(v) {
                            out.push(spot(SpotAt::Arg(i), n, &format!("{f}={n}="), value, ""));
                        }
                    }
                    None if sensitive_name(flag) => {
                        if let Some(Value::String(v)) = args.get(i + 1)
                            && !v.starts_with('-')
                            && !v.is_empty()
                        {
                            out.push(spot(
                                SpotAt::Arg(i + 1),
                                flag.trim_start_matches('-'),
                                "",
                                v,
                                "",
                            ));
                            taken_next = true;
                        }
                    }
                    None => {}
                }
            } else if let Some((n, value)) = named_secret(a) {
                out.push(spot(SpotAt::Arg(i), n, &format!("{n}="), value, ""));
            }
            // A DSN's password, where nothing above took the whole value already.
            if out.len() == before
                && let Some(info) = userinfo(a)
            {
                out.push(spot(
                    SpotAt::Arg(i),
                    &format!("arg{i}_userinfo"),
                    "//",
                    info,
                    "@",
                ));
            }
        }
    }
    out
}

/// One place [`secret_spots`] found a secret: `value` stands between `prefix` and `suffix` in
/// the string at `at`, and a placeholder takes its place there.
#[derive(Debug, Clone, PartialEq)]
pub struct SecretSpot {
    pub at: SpotAt,
    /// The env var, header, parameter or flag it belongs to.
    pub name: String,
    pub prefix: String,
    /// The secret itself.
    pub value: String,
    pub suffix: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpotAt {
    /// `config.env[name]`, the whole string.
    Env,
    /// `config.headers[name]`, the whole string.
    Header,
    /// Inside `config.url`.
    Url,
    /// Inside `config.args[i]`.
    Arg(usize),
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

    #[test]
    fn secrets_are_found_in_env_headers_urls_and_flags() {
        let config = serde_json::json!({
            "command": "docker",
            "args": [
                "run", "-y", "server", "--api-key", "k-1", "--token=t-2", "--verbose", "--port", "80",
                "postgresql://dbuser:p-6@db.local:5432/app",
                "-e", "GITHUB_TOKEN=g-7",
                "--env=SLACK_BOT_TOKEN=s-8",
                "API_SECRET=a-9",
                "MODE=fast",
            ],
            "url": "https://u:w-10@h/mcp?team=a&access_token=q-3#frag",
            "env": {"A": "e-4"},
            "headers": {"Authorization": "Bearer h-5"},
        });
        let spots = secret_spots(&config);
        let mut values: Vec<&str> = spots.iter().map(|s| s.value.as_str()).collect();
        values.sort();
        assert_eq!(
            values,
            [
                "Bearer h-5",
                "a",
                "a-9",
                "dbuser:p-6",
                "e-4",
                "g-7",
                "k-1",
                "q-3",
                "s-8",
                "t-2",
                "u:w-10"
            ],
            "every query value counts, `team=a` included"
        );
        // Each sits exactly between its prefix and suffix in its string.
        for s in &spots {
            let text = match &s.at {
                SpotAt::Env => config["env"][&s.name].as_str().unwrap().to_owned(),
                SpotAt::Header => config["headers"][&s.name].as_str().unwrap().to_owned(),
                SpotAt::Url => config["url"].as_str().unwrap().to_owned(),
                SpotAt::Arg(i) => config["args"][*i].as_str().unwrap().to_owned(),
            };
            assert!(
                text.contains(&format!("{}{}{}", s.prefix, s.value, s.suffix)),
                "{s:?} in {text}"
            );
        }
        // A username alone, or a host, is not a secret.
        let plain = serde_json::json!({"args": ["ssh://git@github.com/o/r", "https://h/x"]});
        assert!(secret_spots(&plain).is_empty());
    }
}
