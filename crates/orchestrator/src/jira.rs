//! Jira, per project, reached only through the orchestrator.
//!
//! The credential (an Atlassian email and API token, with the site URL, in a secrets-manager
//! folder) stays here: it is never written to a node, never put in a session's environment and
//! never shown to the brain, which gets tools instead. That is deliberate — the brain is
//! steerable by untrusted content and has a shell on its machine, so a token there would be
//! write access to the whole tracker for anything that can talk it into using it (see
//! `docs/plan.md`).
//!
//! Everything is scoped to the project's own Jira key: an issue of another project is refused
//! and every search is wrapped in `project = KEY AND (...)`, so one project's agents cannot
//! read or change another's tracker.
//!
//! Jira text is written by whoever can file a ticket, so what comes back is framed as untrusted
//! data, exactly like a feed message.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{chat::clip, core::Orchestrator};

/// The keys the `jira` secret-folder preset defines (see `admin::PRESETS`).
pub const URL_KEY: &str = "JIRA_URL";
pub const EMAIL_KEY: &str = "JIRA_EMAIL";
pub const TOKEN_KEY: &str = "JIRA_API_TOKEN";
/// Where the credential lives unless a project names another folder: one Atlassian account
/// normally covers every project, so only the key (and perhaps the board) differs per project.
pub const DEFAULT_FOLDER: &str = "jira";

const TIMEOUT: Duration = Duration::from_secs(20);
/// Descriptions and comments can be enormous; an agent needs the gist, not the novel.
const MAX_TEXT: usize = 6000;
const MAX_RESULTS: i64 = 50;

/// One project's Jira: where it is, who we are, which Jira project the project maps to, and the
/// board whose sprint counts as "my sprint" (when one was set).
pub struct Jira {
    base: String,
    email: String,
    token: String,
    key: String,
    board: Option<i64>,
    http: reqwest::Client,
}

/// The Jira of `project`, if the owner configured one. The credential comes from the project's
/// own folder when it names one, else from the fleet's shared [`DEFAULT_FOLDER`], so one token
/// serves every project and only the key and board differ.
pub async fn for_project(o: &Orchestrator, project: &str) -> Result<Jira> {
    let row: Option<(Option<String>, Option<String>, Option<i32>)> =
        sqlx::query_as("select jira_folder, jira_key, jira_board from projects where slug = $1")
            .bind(project)
            .fetch_optional(&o.db)
            .await?;
    let (folder, key, board) = row.with_context(|| format!("no project {project}"))?;
    let Some(key) = key else {
        bail!(
            "project {project} has no Jira key yet: the owner sets it (BIOINS, DAP, ...) on the project's page in the web UI"
        );
    };
    let folder = folder.unwrap_or_else(|| DEFAULT_FOLDER.to_owned());
    let missing = |what: &str| {
        format!(
            "the Jira folder {folder:?} of project {project} has no {what}; the owner sets it in the web UI"
        )
    };
    let base = o
        .secret(&folder, URL_KEY)
        .await?
        .with_context(|| missing(URL_KEY))?;
    let email = o
        .secret(&folder, EMAIL_KEY)
        .await?
        .with_context(|| missing(EMAIL_KEY))?;
    let token = o
        .secret(&folder, TOKEN_KEY)
        .await?
        .with_context(|| missing(TOKEN_KEY))?;
    Ok(Jira {
        base: base.trim().trim_end_matches('/').to_owned(),
        email,
        token,
        key: key.trim().to_uppercase(),
        board: board.map(i64::from),
        http: http()?,
    })
}

/// The Jira account a credential folder holds, for what belongs to the account rather than to one
/// project: who it is, the site's fields, a sprint. It has no project key, so it is never used
/// for an issue (every issue call checks the key).
pub async fn for_folder(o: &Orchestrator, folder: &str) -> Result<Jira> {
    let get = |k: &'static str| async move {
        o.secret(folder, k).await?.with_context(|| {
            format!("the Jira folder {folder:?} has no {k}; the owner sets it in the web UI")
        })
    };
    Ok(Jira {
        base: get(URL_KEY).await?.trim().trim_end_matches('/').to_owned(),
        email: get(EMAIL_KEY).await?,
        token: get(TOKEN_KEY).await?,
        key: String::new(),
        board: None,
        http: http()?,
    })
}

/// An HTTP client for Jira. Like every other client in the fleet, it installs the crypto
/// provider first (idempotent: the first caller wins and the rest are no-ops).
fn http() -> Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder().timeout(TIMEOUT).build()?)
}

/// Whether `key` is an issue of this project (`BIOINS-5702` for key `BIOINS`).
pub fn is_own_key(project_key: &str, issue: &str) -> bool {
    let issue = issue.trim().to_uppercase();
    match issue.split_once('-') {
        Some((k, n)) => {
            k == project_key.to_uppercase()
                && !n.is_empty()
                && n.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// `project = KEY AND (<what the caller asked for>)`, so a search cannot wander into another
/// project's tracker. A trailing `ORDER BY` is split off and re-appended, because JQL does not
/// allow one inside parentheses. `to_ascii_uppercase` keeps byte offsets, which a full
/// `to_uppercase` would not for non-ASCII text in a search.
pub fn scoped_jql(project_key: &str, jql: &str) -> String {
    let jql = jql.trim().trim_end_matches(';').trim();
    let (predicate, order) = match jql.to_ascii_uppercase().rfind("ORDER BY") {
        Some(i) => (jql[..i].trim(), Some(jql[i..].trim())),
        None => (jql, None),
    };
    let mut out = if predicate.is_empty() {
        format!("project = {project_key}")
    } else {
        format!("project = {project_key} AND ({predicate})")
    };
    if let Some(order) = order.filter(|o| !o.is_empty()) {
        out.push(' ');
        out.push_str(order);
    }
    out
}

/// What "my sprint" means: one board's active sprint when there is one, else every active sprint
/// of the project. Jira's `openSprints()` also holds the sprints not started yet, so those are
/// taken out: a ticket of next sprint is not this sprint's work (and the roadmap would start it).
pub fn sprint_jql(sprint: Option<i64>) -> String {
    let which = match sprint {
        Some(id) => format!("sprint = {id}"),
        None => "sprint in openSprints() AND sprint not in futureSprints()".into(),
    };
    format!("{which} AND assignee = currentUser() ORDER BY status, priority DESC")
}

/// The transition whose name the caller meant, from what Jira offers for that issue.
pub fn transition_id(transitions: &Value, name: &str) -> Result<String> {
    let want = name.trim().to_lowercase();
    let list = transitions["transitions"]
        .as_array()
        .context("Jira returned no transitions")?;
    let named: Vec<String> = list
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    let found = list.iter().find(|t| {
        t["name"].as_str().map(str::to_lowercase).as_deref() == Some(want.as_str())
            || t["to"]["name"].as_str().map(str::to_lowercase).as_deref() == Some(want.as_str())
    });
    let id = found
        .and_then(|t| t["id"].as_str())
        .with_context(|| format!("{name:?} is not a transition this issue offers: {named:?}"))?;
    Ok(id.to_owned())
}

impl Jira {
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Enough of the account to tell which one it is, without handing out the value itself. The
    /// email is a secret-folder item like any other, so it never reaches an agent whole; the
    /// domain is what tells a wrong account from a wrong site anyway.
    fn masked(email: &str) -> String {
        match email.split_once('@') {
            Some((user, domain)) => format!(
                "{}…@{domain}",
                user.chars().take(2).collect::<String>().trim()
            ),
            None => "…".to_owned(),
        }
    }

    /// The site's host, for saying where a call went without repeating the whole URL.
    pub fn site(&self) -> &str {
        self.base
            .trim_start_matches("https://")
            .trim_start_matches("http://")
    }

    /// Where this project's Jira points, who the fleet is there, and whether that account can
    /// actually see the project. The answer to "why did it say 404": a wrong site, an account
    /// without access and a mistyped key all look alike from a single failed read.
    pub async fn whoami(&self) -> Result<Value> {
        let me = self.call(reqwest::Method::GET, "myself", None).await?;
        // A search the project scope makes safe: it either returns issues, or says why not.
        let visible = match self.search("", 1).await {
            Ok(v) => json!({"can_see_the_project": true, "read": v["read"].clone()}),
            Err(e) => json!({"can_see_the_project": false, "why": e.to_string()}),
        };
        Ok(json!({
            "site": self.site(),
            // Who the site says we are, by name, plus the configured address masked: the name
            // is Jira's own answer and identifies the account, the address is a vault value.
            "account": Self::masked(me["emailAddress"].as_str().unwrap_or(&self.email)),
            "account_name": me["displayName"].clone(),
            "project_key": self.key,
            "board": self.board,
            "project": visible,
        }))
    }

    /// Issues, comments and transitions go through **v2**: Cloud still serves it, and it reads
    /// and writes descriptions and comment bodies as plain strings instead of Atlassian
    /// Document Format trees, which is what an agent can actually read and write.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        self.call_at(method, &format!("rest/api/2/{path}"), body)
            .await
    }

    /// A call to an exact path under the site, for the endpoints that are not under v2.
    async fn call_at(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut req = self
            .http
            .request(method, format!("{}/{path}", self.base))
            .basic_auth(&self.email, Some(&self.token))
            .header("Accept", "application/json");
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req.send().await.context("Jira could not be reached")?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        ensure!(
            status.is_success(),
            // Jira's own words, which say what is wrong far better than a status alone. No
            // credential is ever part of a response.
            //
            // For the three statuses that mean "not you": which site and which account, because
            // Jira's own answer to both a wrong site and an account without access is the same
            // 404 ("Issue does not exist or you do not have permission to see it"), and without
            // this the reader cannot tell a typo in the folder's URL from a missing permission.
            // The token never appears; the email is the owner's own, in the owner's own fleet.
            "Jira answered {}{}: {}",
            status.as_u16(),
            if matches!(status.as_u16(), 401 | 403 | 404) {
                format!(" (site {}, as {})", self.site(), Self::masked(&self.email))
            } else {
                String::new()
            },
            clip(text.trim(), 600)
        );
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&text).context("Jira sent something that is not JSON")
    }

    /// One issue, as an agent needs it: what it is, where it stands, and who has it.
    pub async fn issue(&self, key: &str) -> Result<Value> {
        self.check(key)?;
        let v = self
            .call(
                reqwest::Method::GET,
                &format!(
                    "issue/{key}?fields=summary,description,status,assignee,reporter,issuetype,priority,labels,parent,duedate"
                ),
                None,
            )
            .await?;
        Ok(brief(&v))
    }

    /// The issues a JQL finds, scoped to this project.
    ///
    /// This one endpoint is **v3**, not v2: Cloud removed `/rest/api/{2,3}/search` outright
    /// (it answers 410 pointing at the migration), and `/rest/api/3/search/jql` is the
    /// replacement. Mixing versions is safe here because none of the fields asked for is rich
    /// text — the only fields that differ between v2 and v3 are ones like `description`, which
    /// a search never returns. Paging is by `nextPageToken` now, and the response carries no
    /// total, so what comes back says how many were read and whether more remain.
    pub async fn search(&self, jql: &str, limit: i64) -> Result<Value> {
        let jql = scoped_jql(&self.key, jql);
        let v = self
            .call_at(
                reqwest::Method::POST,
                "rest/api/3/search/jql",
                Some(json!({
                    "jql": jql,
                    "maxResults": limit.clamp(1, MAX_RESULTS),
                    "fields": ["summary", "status", "assignee", "issuetype", "priority", "duedate"],
                })),
            )
            .await?;
        let issues: Vec<Value> = v["issues"].as_array().cloned().unwrap_or_default();
        Ok(json!({
            "jql": jql,
            "read": issues.len(),
            "more": v["nextPageToken"].is_string(),
            "issues": issues.iter().map(brief).collect::<Vec<_>>(),
        }))
    }

    /// What is in the owner's current sprint for this project. With a board configured the
    /// active sprint of that board is used, which is what "my sprint" means when a project has
    /// more than one board running; without one, every open sprint of the project counts.
    pub async fn sprint(&self, limit: i64) -> Result<Value> {
        let jql = match self.active_sprint().await? {
            Some(id) => sprint_jql(Some(id)),
            None => sprint_jql(None),
        };
        self.search(&jql, limit).await
    }

    /// Who this account is on its site: the account id (what "assigned to me" means there), its
    /// name and its time zone.
    pub async fn myself(&self) -> Result<JiraUser> {
        let v = self
            .call_at(reqwest::Method::GET, "rest/api/3/myself", None)
            .await?;
        user_of(&v).context("Jira did not say who this account is")
    }

    /// The site's sprint field and rank field. Their ids differ from site to site
    /// (`customfield_10020` on one, something else on another), so they are looked up by what
    /// they are, preferring the field named "Sprint" when a site has more than one.
    pub async fn discover_fields(&self) -> Result<(Option<String>, Option<String>)> {
        let v = self
            .call_at(reqwest::Method::GET, "rest/api/3/field", None)
            .await?;
        Ok(fields_of(&v))
    }

    /// One sprint, from the agile API: its state and dates may change without any of its
    /// issues changing (a sprint starting), so open sprints are asked about themselves.
    pub async fn sprint_info(&self, id: i64) -> Result<SprintInfo> {
        let v = self
            .call_at(
                reqwest::Method::GET,
                &format!("rest/agile/1.0/sprint/{id}"),
                None,
            )
            .await?;
        sprint_of(&v).context("Jira sent a sprint without an id")
    }

    /// The issues of this project for the local copy (`jira_sync`), as `read` says, at most `cap`
    /// of them, and whether that was all of them: `false` whenever Jira still had a page to give
    /// (a read stopped short is never taken for a complete one). Only this project's own tickets
    /// are kept, whatever came back.
    pub async fn mirror_issues(
        &self,
        read: MirrorRead,
        sprint_field: Option<&str>,
        rank_field: Option<&str>,
        cap: usize,
    ) -> Result<(Vec<MirrorIssue>, bool)> {
        let jql = scoped_jql(&self.key, &mirror_jql(read, sprint_field.is_some()));
        let mut fields: Vec<&str> = vec![
            "summary",
            "status",
            "issuetype",
            "priority",
            "labels",
            "parent",
            "assignee",
            "reporter",
            "created",
            "updated",
        ];
        fields.extend(sprint_field);
        fields.extend(rank_field);
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        // Bounded by what was kept, and by a page count no honest read reaches (Jira may hand out
        // fewer than asked per page), so a token that never ends cannot hold the sync for ever.
        for _ in 0..MIRROR_MAX_PAGES {
            let mut body = json!({"jql": jql, "maxResults": MIRROR_PAGE, "fields": fields});
            if let Some(tok) = &page {
                body["nextPageToken"] = json!(tok);
            }
            let v = self
                .call_at(reqwest::Method::POST, "rest/api/3/search/jql", Some(body))
                .await?;
            for i in v["issues"].as_array().into_iter().flatten() {
                if let Some(m) = mirror_of(i, sprint_field, rank_field)
                    && is_own_key(&self.key, &m.key)
                {
                    out.push(m);
                }
            }
            page = v["nextPageToken"].as_str().map(str::to_owned);
            if page.is_none() || out.len() >= cap {
                break;
            }
        }
        let complete = page.is_none() && out.len() <= cap;
        out.truncate(cap);
        Ok((out, complete))
    }

    /// Who commented on an issue lately: the authors only, never what they wrote.
    pub async fn comment_authors(&self, key: &str, n: i64) -> Result<Vec<JiraUser>> {
        self.check(key)?;
        let v = self
            .call_at(
                reqwest::Method::GET,
                &format!(
                    "rest/api/3/issue/{key}/comment?maxResults={}&orderBy=-created",
                    n.clamp(1, 50)
                ),
                None,
            )
            .await?;
        Ok(v["comments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| user_of(&c["author"]))
            .collect())
    }

    /// Where a person reads an issue.
    pub fn browse_url(&self, key: &str) -> String {
        format!("{}/browse/{key}", self.base)
    }

    /// The id of the active sprint of this project's board, if a board was configured.
    async fn active_sprint(&self) -> Result<Option<i64>> {
        let Some(board) = self.board else {
            return Ok(None);
        };
        // The agile API is a different base path from the issue API.
        let url = format!(
            "{}/rest/agile/1.0/board/{board}/sprint?state=active",
            self.base
        );
        let res = self
            .http
            .get(url)
            .basic_auth(&self.email, Some(&self.token))
            .header("Accept", "application/json")
            .send()
            .await
            .context("Jira could not be reached")?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        ensure!(
            status.is_success(),
            "Jira answered {} for board {board}: {}",
            status.as_u16(),
            clip(text.trim(), 300)
        );
        let v: Value =
            serde_json::from_str(&text).context("Jira sent something that is not JSON")?;
        Ok(v["values"]
            .as_array()
            .and_then(|s| s.first())
            .and_then(|s| s["id"].as_i64()))
    }

    pub async fn comment(&self, key: &str, body: &str) -> Result<Value> {
        self.check(key)?;
        ensure!(!body.trim().is_empty(), "the comment is empty");
        let v = self
            .call(
                reqwest::Method::POST,
                &format!("issue/{key}/comment"),
                Some(json!({"body": clip(body, MAX_TEXT)})),
            )
            .await?;
        Ok(json!({"issue": key, "comment": v["id"]}))
    }

    /// Moves an issue by the name of the transition (or of the status it leads to), so a caller
    /// can say "In Review" instead of looking up Jira's numeric id.
    pub async fn transition(&self, key: &str, name: &str) -> Result<Value> {
        self.check(key)?;
        let offered = self
            .call(
                reqwest::Method::GET,
                &format!("issue/{key}/transitions"),
                None,
            )
            .await?;
        let id = transition_id(&offered, name)?;
        self.call(
            reqwest::Method::POST,
            &format!("issue/{key}/transitions"),
            Some(json!({"transition": {"id": id}})),
        )
        .await?;
        Ok(json!({"issue": key, "moved_to": name}))
    }

    /// Moves an issue by the first of `names` its workflow offers (a transition or the status it
    /// leads to), since workflows name the same step differently ("In Review", "Code Review").
    /// Returns the name used.
    pub async fn transition_any(&self, key: &str, names: &[&str]) -> Result<String> {
        self.check(key)?;
        let offered = self
            .call(
                reqwest::Method::GET,
                &format!("issue/{key}/transitions"),
                None,
            )
            .await?;
        let (name, id) = names
            .iter()
            .find_map(|n| transition_id(&offered, n).ok().map(|id| (*n, id)))
            .with_context(|| {
                let named: Vec<&str> = offered["transitions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t["name"].as_str())
                    .collect();
                format!("{key}'s workflow offers none of {names:?} now (it offers {named:?})")
            })?;
        self.call(
            reqwest::Method::POST,
            &format!("issue/{key}/transitions"),
            Some(json!({"transition": {"id": id}})),
        )
        .await?;
        Ok(name.to_owned())
    }

    /// A new issue in this project. Only ever run once the owner has approved it (see `admin`).
    pub async fn create(
        &self,
        summary: &str,
        description: &str,
        issue_type: &str,
        parent: Option<&str>,
    ) -> Result<Value> {
        ensure!(!summary.trim().is_empty(), "the summary is empty");
        let mut fields = json!({
            "project": {"key": self.key},
            "summary": clip(summary.trim(), 250),
            "description": clip(description, MAX_TEXT),
            "issuetype": {"name": issue_type},
        });
        if let Some(parent) = parent {
            self.check(parent)?;
            fields["parent"] = json!({"key": parent.to_uppercase()});
        }
        let v = self
            .call(
                reqwest::Method::POST,
                "issue",
                Some(json!({"fields": fields})),
            )
            .await?;
        Ok(
            json!({"created": v["key"], "url": format!("{}/browse/{}", self.base, v["key"].as_str().unwrap_or_default())}),
        )
    }

    /// Changes fields of an existing issue. Only ever run once the owner has approved it.
    pub async fn update(
        &self,
        key: &str,
        summary: Option<&str>,
        description: Option<&str>,
    ) -> Result<Value> {
        self.check(key)?;
        let mut fields = serde_json::Map::new();
        if let Some(s) = summary {
            fields.insert("summary".into(), json!(clip(s.trim(), 250)));
        }
        if let Some(d) = description {
            fields.insert("description".into(), json!(clip(d, MAX_TEXT)));
        }
        ensure!(!fields.is_empty(), "nothing to change");
        self.call(
            reqwest::Method::PUT,
            &format!("issue/{key}"),
            Some(json!({"fields": Value::Object(fields)})),
        )
        .await?;
        Ok(json!({"issue": key, "updated": true}))
    }

    fn check(&self, issue: &str) -> Result<()> {
        ensure!(
            is_own_key(&self.key, issue),
            "{issue} is not an issue of this project's Jira ({}-…)",
            self.key
        );
        Ok(())
    }
}

/// Issues per page of the local copy's reads.
const MIRROR_PAGE: usize = 100;
/// The most pages one read asks for.
const MIRROR_MAX_PAGES: usize = 200;

/// What one read of the local copy asks Jira for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorRead {
    /// Everything updated in the last minutes, whatever its state: a ticket that closed comes
    /// back closed.
    Recent(i64),
    /// Every ticket of an open sprint (active or future), done or not: the part of a full read
    /// that a cap never cuts first, so the sprints the owner works from are whole.
    OpenSprints,
    /// Every ticket still open outside the open sprints, the most recently updated first: the
    /// rest of a full read, which a cap cuts at the stalest end.
    Backlog,
}

/// The JQL of a read. A site without sprints (no sprint field) has no open sprints to ask about.
pub fn mirror_jql(read: MirrorRead, has_sprints: bool) -> String {
    match (read, has_sprints) {
        (MirrorRead::Recent(m), _) => format!("updated >= -{}m ORDER BY updated DESC", m.max(1)),
        (MirrorRead::OpenSprints, _) => "sprint in openSprints() ORDER BY Rank".into(),
        (MirrorRead::Backlog, true) => "statusCategory != Done AND (sprint is EMPTY OR sprint not in openSprints()) ORDER BY updated DESC".into(),
        (MirrorRead::Backlog, false) => "statusCategory != Done ORDER BY updated DESC".into(),
    }
}

/// Someone on a Jira site, as the local copy keeps them. Their name is theirs to choose: shown as
/// text, framed as data wherever an agent reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct JiraUser {
    pub account_id: String,
    pub display_name: String,
    pub time_zone: Option<String>,
    pub avatar_url: Option<String>,
}

fn user_of(v: &Value) -> Option<JiraUser> {
    Some(JiraUser {
        account_id: v["accountId"].as_str()?.to_owned(),
        display_name: v["displayName"].as_str().unwrap_or("").to_owned(),
        time_zone: v["timeZone"].as_str().map(str::to_owned),
        avatar_url: v["avatarUrls"]["48x48"].as_str().map(str::to_owned),
    })
}

/// A sprint: `active`, `future` or `closed`, with its board and dates when it has them.
#[derive(Debug, Clone, PartialEq)]
pub struct SprintInfo {
    pub id: i64,
    pub name: String,
    pub state: String,
    pub board_id: Option<i64>,
    pub goal: Option<String>,
    pub starts_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ends_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn date(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .or_else(|| chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.3f%z").ok())
        .map(|d| d.with_timezone(&chrono::Utc))
}

fn sprint_of(v: &Value) -> Option<SprintInfo> {
    Some(SprintInfo {
        id: v["id"].as_i64()?,
        name: v["name"].as_str().unwrap_or("").to_owned(),
        state: v["state"].as_str().unwrap_or("").to_lowercase(),
        board_id: v["boardId"].as_i64().or(v["originBoardId"].as_i64()),
        goal: v["goal"]
            .as_str()
            .filter(|g| !g.is_empty())
            .map(str::to_owned),
        starts_at: date(&v["startDate"]),
        ends_at: date(&v["endDate"]),
    })
}

/// The sprint an issue is in now: an active one before a future one (the soonest), else none,
/// which means the backlog. Closed sprints are history.
pub fn pick_open_sprint(sprints: &[SprintInfo]) -> Option<&SprintInfo> {
    sprints.iter().find(|s| s.state == "active").or_else(|| {
        sprints
            .iter()
            .filter(|s| s.state == "future")
            .min_by_key(|s| (s.starts_at.is_none(), s.starts_at, s.id))
    })
}

/// The sprint and rank fields among a site's fields.
fn fields_of(v: &Value) -> (Option<String>, Option<String>) {
    let all = v.as_array().cloned().unwrap_or_default();
    let of = |custom: &str| -> Vec<&Value> {
        all.iter()
            .filter(|f| f["schema"]["custom"].as_str() == Some(custom))
            .collect()
    };
    let sprints = of("com.pyxis.greenhopper.jira:gh-sprint");
    let sprint = sprints
        .iter()
        .find(|f| f["name"].as_str() == Some("Sprint"))
        .or(sprints.first())
        .and_then(|f| f["id"].as_str().map(str::to_owned));
    let rank = of("com.pyxis.greenhopper.jira:gh-lexo-rank")
        .first()
        .and_then(|f| f["id"].as_str().map(str::to_owned));
    (sprint, rank)
}

/// An issue as the local copy keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct MirrorIssue {
    pub key: String,
    pub id: Option<String>,
    pub summary: String,
    pub status: Option<String>,
    pub status_category: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<String>,
    pub labels: Vec<String>,
    pub parent: Option<String>,
    pub assignee: Option<JiraUser>,
    pub reporter: Option<JiraUser>,
    /// Every sprint it has been in that Jira names, and the open one it is in now.
    pub sprints: Vec<SprintInfo>,
    pub sprint: Option<SprintInfo>,
    pub rank: Option<String>,
    pub created: Option<chrono::DateTime<chrono::Utc>>,
    pub updated: Option<chrono::DateTime<chrono::Utc>>,
}

fn mirror_of(
    i: &Value,
    sprint_field: Option<&str>,
    rank_field: Option<&str>,
) -> Option<MirrorIssue> {
    let f = &i["fields"];
    let key = i["key"].as_str()?.to_owned();
    let sprints: Vec<SprintInfo> = sprint_field
        .and_then(|sf| f[sf].as_array())
        .into_iter()
        .flatten()
        .filter_map(sprint_of)
        .collect();
    let sprint = pick_open_sprint(&sprints).cloned();
    Some(MirrorIssue {
        summary: clip(f["summary"].as_str().unwrap_or(&key), 500),
        key,
        id: i["id"].as_str().map(str::to_owned),
        status: f["status"]["name"].as_str().map(str::to_owned),
        status_category: f["status"]["statusCategory"]["key"]
            .as_str()
            .map(str::to_owned),
        issue_type: f["issuetype"]["name"].as_str().map(str::to_owned),
        priority: f["priority"]["name"].as_str().map(str::to_owned),
        labels: f["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().map(str::to_owned))
            .collect(),
        parent: f["parent"]["key"].as_str().map(str::to_owned),
        assignee: user_of(&f["assignee"]),
        reporter: user_of(&f["reporter"]),
        sprints,
        sprint,
        rank: rank_field.and_then(|r| f[r].as_str()).map(str::to_owned),
        created: date(&f["created"]),
        updated: date(&f["updated"]),
    })
}

/// The fields of an issue worth an agent's tokens, with its text clipped.
fn brief(v: &Value) -> Value {
    let f = &v["fields"];
    let mut out = json!({
        "key": v["key"],
        "summary": f["summary"],
        "status": f["status"]["name"],
        "type": f["issuetype"]["name"],
        "assignee": f["assignee"]["displayName"],
        "reporter": f["reporter"]["displayName"],
        "priority": f["priority"]["name"],
        "due": f["duedate"],
    });
    if let Some(d) = f["description"].as_str().filter(|d| !d.trim().is_empty()) {
        out["description"] = json!(clip(d, MAX_TEXT));
    }
    if let Some(p) = f["parent"]["key"].as_str() {
        out["parent"] = json!(p);
    }
    if let Some(labels) = f["labels"].as_array().filter(|l| !l.is_empty()) {
        out["labels"] = json!(labels);
    }
    out
}

#[cfg(test)]
impl Jira {
    /// A client pointed at a stand-in Jira, so the request and response shapes are tested
    /// without a database or the real Atlassian.
    fn for_test(base: &str, key: &str, board: Option<i64>) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            email: "me@x.test".into(),
            token: "tok".into(),
            key: key.to_owned(),
            board,
            http: http().unwrap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path, query_param},
    };

    /// What `basic_auth` puts on every request. No credential is logged anywhere else.
    fn basic() -> String {
        format!("Basic {}", data_encoding::BASE64.encode(b"me@x.test:tok"))
    }

    #[tokio::test]
    async fn an_issue_is_read_with_the_credential_and_trimmed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/2/issue/BIOINS-5702"))
            .and(header("authorization", basic().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "key": "BIOINS-5702",
                "fields": {
                    "summary": "File ownership not enforced",
                    "description": "A user can reach another user's files.",
                    "status": {"name": "To Do"},
                    "issuetype": {"name": "Bug"},
                    "assignee": {"displayName": "Bohdan"},
                    "reporter": {"displayName": "Liana"},
                    "priority": {"name": "High"},
                    "parent": {"key": "BIOINS-5753"},
                    "labels": ["security"],
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let j = Jira::for_test(&server.uri(), "BIOINS", None);
        let out = j.issue("BIOINS-5702").await.unwrap();
        assert_eq!(out["status"], "To Do");
        assert_eq!(out["parent"], "BIOINS-5753");
        assert!(
            out["description"]
                .as_str()
                .unwrap()
                .contains("another user's files")
        );
        // Another project's tracker is refused before any request is made.
        assert!(j.issue("DAP-80").await.is_err());
    }

    #[tokio::test]
    async fn a_search_is_sent_scoped_to_the_project() {
        let server = MockServer::start().await;
        // The new endpoint: Cloud removed /rest/api/{2,3}/search outright.
        Mock::given(method("POST"))
            .and(path("/rest/api/3/search/jql"))
            .and(body_json(json!({
                "jql": "project = DAP AND (assignee = currentUser())",
                "maxResults": 5,
                "fields": ["summary", "status", "assignee", "issuetype", "priority", "duedate"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [{"key": "DAP-80", "fields": {"summary": "Git integration", "status": {"name": "In Progress"}}}],
                "nextPageToken": "more-please",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let out = Jira::for_test(&server.uri(), "DAP", None)
            .search("assignee = currentUser()", 5)
            .await
            .unwrap();
        // There is no total any more: what came back, and whether a page follows.
        assert_eq!(out["read"], 1);
        assert_eq!(out["more"], true);
        assert_eq!(out["issues"][0]["key"], "DAP-80");
    }

    #[tokio::test]
    async fn searching_never_calls_the_removed_endpoint() {
        // The regression this guards: /rest/api/2/search answered 410 in production, telling us
        // to migrate. Nothing may call it again.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/search/jql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issues": []})))
            .mount(&server)
            .await;
        let j = Jira::for_test(&server.uri(), "BIOINS", None);
        j.search("assignee = currentUser()", 5).await.unwrap();
        j.sprint(5).await.unwrap();
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.url.path() == "/rest/api/3/search/jql"),
            "every search goes to the replacement endpoint"
        );
        // Without a total there is nothing to report but what was read.
        let out = j.search("assignee = currentUser()", 5).await.unwrap();
        assert_eq!(out["read"], 0);
        assert_eq!(out["more"], false);
    }

    #[tokio::test]
    async fn my_sprint_asks_the_board_for_its_active_sprint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/board/7/sprint"))
            .and(query_param("state", "active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [{"id": 99, "name": "Sprint 14"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/api/3/search/jql"))
            .and(body_json(json!({
                "jql": "project = BIOINS AND (sprint = 99 AND assignee = currentUser()) ORDER BY status, priority DESC",
                "maxResults": 20,
                "fields": ["summary", "status", "assignee", "issuetype", "priority", "duedate"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issues": []})))
            .expect(1)
            .mount(&server)
            .await;
        Jira::for_test(&server.uri(), "BIOINS", Some(7))
            .sprint(20)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_transition_is_looked_up_by_name_then_posted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/2/issue/BIOINS-1/transitions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "transitions": [{"id": "31", "name": "Review", "to": {"name": "In Review"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/issue/BIOINS-1/transitions"))
            .and(body_json(json!({"transition": {"id": "31"}})))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let out = Jira::for_test(&server.uri(), "BIOINS", None)
            .transition("BIOINS-1", "In Review")
            .await
            .unwrap();
        assert_eq!(out["moved_to"], "In Review");
    }

    #[tokio::test]
    async fn commenting_and_creating_send_plain_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/issue/BIOINS-1/comment"))
            .and(body_json(json!({"body": "Looked at it."})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "10001"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/issue"))
            .and(body_json(json!({"fields": {
                "project": {"key": "BIOINS"},
                "summary": "Fix it",
                "description": "Why",
                "issuetype": {"name": "Bug"},
                "parent": {"key": "BIOINS-5753"},
            }})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"key": "BIOINS-9000"})))
            .expect(1)
            .mount(&server)
            .await;
        let j = Jira::for_test(&server.uri(), "BIOINS", None);
        assert_eq!(
            j.comment("BIOINS-1", "Looked at it.").await.unwrap()["comment"],
            "10001"
        );
        let made = j
            .create("Fix it", "Why", "Bug", Some("BIOINS-5753"))
            .await
            .unwrap();
        assert_eq!(made["created"], "BIOINS-9000");
        assert!(
            made["url"]
                .as_str()
                .unwrap()
                .ends_with("/browse/BIOINS-9000")
        );
    }

    #[tokio::test]
    async fn jiras_own_complaint_is_what_the_agent_reads() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/2/issue/BIOINS-1"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "errorMessages": ["Issue does not exist or you do not have permission to see it."]
            })))
            .mount(&server)
            .await;
        let e = Jira::for_test(&server.uri(), "BIOINS", None)
            .issue("BIOINS-1")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("404") && e.contains("do not have permission"),
            "{e}"
        );
    }

    #[test]
    fn the_sprint_field_is_found_by_what_it_is() {
        let fields = json!([
            {"id": "customfield_1", "name": "Old sprint", "schema": {"custom": "com.pyxis.greenhopper.jira:gh-sprint"}},
            {"id": "customfield_10020", "name": "Sprint", "schema": {"custom": "com.pyxis.greenhopper.jira:gh-sprint"}},
            {"id": "customfield_10019", "name": "Rank", "schema": {"custom": "com.pyxis.greenhopper.jira:gh-lexo-rank"}},
            {"id": "summary", "name": "Summary", "schema": {"system": "summary"}},
        ]);
        assert_eq!(
            fields_of(&fields),
            (
                Some("customfield_10020".into()),
                Some("customfield_10019".into())
            )
        );
        assert_eq!(fields_of(&json!([])), (None, None));
    }

    #[test]
    fn an_issue_is_in_its_active_sprint_else_its_next_one_else_the_backlog() {
        let s = |id, state: &str, start: Option<&str>| {
            sprint_of(
                &json!({"id": id, "name": format!("S{id}"), "state": state, "startDate": start}),
            )
            .unwrap()
        };
        let closed = s(1, "closed", Some("2026-11-01T09:00:00.000Z"));
        let active = s(2, "active", Some("2026-11-23T09:00:00.000Z"));
        let later = s(4, "future", Some("2026-12-21T09:00:00.000Z"));
        let next = s(3, "future", Some("2026-12-07T09:00:00.000Z"));
        assert_eq!(
            pick_open_sprint(&[closed.clone(), later.clone(), active.clone()])
                .unwrap()
                .id,
            2
        );
        assert_eq!(
            pick_open_sprint(&[closed.clone(), later, next]).unwrap().id,
            3
        );
        assert_eq!(pick_open_sprint(&[closed]), None);
        assert!(active.starts_at.is_some(), "Jira's dates are read");
    }

    #[test]
    fn the_local_copy_reads_recent_changes_or_everything_open() {
        assert_eq!(
            mirror_jql(MirrorRead::Recent(30), true),
            "updated >= -30m ORDER BY updated DESC"
        );
        assert_eq!(
            scoped_jql("DAP", &mirror_jql(MirrorRead::OpenSprints, true)),
            "project = DAP AND (sprint in openSprints()) ORDER BY Rank"
        );
        assert!(mirror_jql(MirrorRead::Backlog, true).contains("sprint not in openSprints()"));
        assert!(!mirror_jql(MirrorRead::Backlog, false).contains("sprint"));
        let i = json!({"key": "DAP-1", "id": "100", "fields": {
            "summary": "Do it", "status": {"name": "To Do", "statusCategory": {"key": "new"}},
            "assignee": {"accountId": "u-1", "displayName": "Ann", "timeZone": "Europe/Kyiv"},
            "customfield_10020": [{"id": 7, "name": "S7", "state": "future", "boardId": 3}],
            "labels": ["api"],
        }});
        let m = mirror_of(&i, Some("customfield_10020"), None).unwrap();
        assert_eq!(m.assignee.unwrap().account_id, "u-1");
        assert_eq!(m.sprint.unwrap().id, 7);
        assert_eq!(m.labels, vec!["api".to_string()]);
        assert!(m.reporter.is_none());
    }

    #[test]
    fn only_this_projects_issues() {
        assert!(is_own_key("BIOINS", "BIOINS-5702"));
        assert!(is_own_key("BIOINS", "bioins-5702"), "case does not matter");
        assert!(!is_own_key("BIOINS", "DAP-80"), "another project's tracker");
        assert!(!is_own_key("BIOINS", "BIOINS"));
        assert!(!is_own_key("BIOINS", "BIOINS-"));
        assert!(!is_own_key("BIOINS", "BIOINS-5702x"));
        assert!(!is_own_key("BIOINS", ""));
    }

    #[test]
    fn a_search_cannot_leave_the_project() {
        assert_eq!(
            scoped_jql("DAP", "assignee = currentUser()"),
            "project = DAP AND (assignee = currentUser())"
        );
        assert_eq!(scoped_jql("DAP", "  "), "project = DAP");
        // Even a crafted tail stays inside the parentheses.
        assert_eq!(
            scoped_jql("DAP", "project = BIOINS OR text ~ \"x\""),
            "project = DAP AND (project = BIOINS OR text ~ \"x\")"
        );
        // JQL allows no ORDER BY inside parentheses: it goes to the end instead.
        assert_eq!(
            scoped_jql("DAP", "assignee = currentUser() ORDER BY status"),
            "project = DAP AND (assignee = currentUser()) ORDER BY status"
        );
        assert_eq!(
            scoped_jql("DAP", "order by created DESC"),
            "project = DAP order by created DESC"
        );
    }

    #[test]
    fn my_sprint_follows_the_board_when_there_is_one() {
        assert!(sprint_jql(Some(42)).starts_with("sprint = 42 AND assignee = currentUser()"));
        assert!(sprint_jql(None).starts_with(
            "sprint in openSprints() AND sprint not in futureSprints() AND assignee = currentUser()"
        ));
        // Whichever it is, the project scope is added around the predicate only.
        assert_eq!(
            scoped_jql("DAP", &sprint_jql(Some(42))),
            "project = DAP AND (sprint = 42 AND assignee = currentUser()) ORDER BY status, priority DESC"
        );
    }

    #[test]
    fn an_account_is_named_without_being_handed_over() {
        // Enough to tell a wrong account from a wrong site; not the value itself, which is a
        // secret-folder item.
        assert_eq!(Jira::masked("bohdan.lushchyk@devcom.com"), "bo…@devcom.com");
        assert_eq!(Jira::masked("a@x.test"), "a…@x.test");
        assert_eq!(Jira::masked("nonsense"), "…");
    }

    #[test]
    fn transitions_are_found_by_name_or_target_status() {
        let offered = json!({"transitions": [
            {"id": "11", "name": "Start progress", "to": {"name": "In Progress"}},
            {"id": "31", "name": "Review", "to": {"name": "In Review"}},
        ]});
        assert_eq!(transition_id(&offered, "Review").unwrap(), "31");
        assert_eq!(transition_id(&offered, "in review").unwrap(), "31");
        assert_eq!(transition_id(&offered, "Start progress").unwrap(), "11");
        let e = transition_id(&offered, "Done").unwrap_err().to_string();
        assert!(e.contains("Start progress") && e.contains("Review"), "{e}");
    }

    #[test]
    fn an_issue_is_trimmed_to_what_matters() {
        let huge = "x".repeat(MAX_TEXT * 2);
        let v = json!({"key": "BIOINS-1", "fields": {
            "summary": "A ticket", "description": huge,
            "status": {"name": "To Do"}, "issuetype": {"name": "Bug"},
            "assignee": {"displayName": "Bohdan"}, "reporter": {"displayName": "Liana"},
            "priority": {"name": "High"}, "duedate": null,
            "parent": {"key": "BIOINS-5753"}, "labels": ["security"],
        }});
        let b = brief(&v);
        assert_eq!(b["status"], "To Do");
        assert_eq!(b["parent"], "BIOINS-5753");
        assert_eq!(b["labels"], json!(["security"]));
        let d = b["description"].as_str().unwrap();
        assert_eq!(d.chars().count(), MAX_TEXT, "clipped to the cap");
        assert!(d.ends_with('…'), "and says it was cut");
    }
}
