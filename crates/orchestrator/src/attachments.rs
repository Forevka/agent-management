//! What a task produced, or is best read with: titled links and files.
//!
//! A link (`links`) says what a task is *about* in somebody else's tracker. An attachment is one
//! of its deliverables: a claude.ai artifact, a doc, a generated asset, a migration the owner has
//! to hand to devops. It has a title, because finding it again later is the whole point.
//!
//! - **Files of any kind**, stored as uploaded and downloaded byte for byte. They live in Postgres
//!   in chunks of at most [`CHUNK`] (so the existing backup covers them, and no message on the
//!   wire or row in the database ever holds a whole large file), up to
//!   `FLEET_ATTACHMENT_MAX_MB`.
//! - **Uploaded in pieces.** A frame on the link and a request on the node's IPC are both capped
//!   at 4 MiB, so a file arrives as `begin`, one `chunk` per piece, then `finish`. Only `finish`
//!   makes it visible, after checking that every piece arrived, that the bytes hash to what the
//!   sender said, and that none of the project's secret values is inside. An upload that never
//!   finishes is swept after an hour.
//! - **Secrets stay out.** A file that contains a secret value is refused, not redacted:
//!   rewriting bytes would corrupt a binary, and a deliverable that silently differs from what the
//!   agent made is worse than an error the agent can act on. A URL with a secret in it is refused
//!   the same way, and titles lose secret values like every other free text an agent sends.
//! - **Never rendered.** A file is served as a download (`Content-Disposition: attachment`,
//!   `nosniff`), whatever its name says it is, so an HTML or SVG file an agent made can never run
//!   in the owner's session. Only common raster images are shown inline, for a preview.

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::FromRow;

use crate::{
    chat::{Notify, clip, esc},
    core::Orchestrator,
    model::TaskRow,
};

/// The most bytes one chunk may carry: 1 MiB, which is 1.33 MiB as base64 in a tool call —
/// comfortably inside the 4 MiB frame and IPC limits, and inside the web API's 2 MiB body limit.
pub const CHUNK: usize = 1024 * 1024;

/// How long an unfinished upload may wait for its next piece before it is thrown away.
const STALE_UPLOAD_MINUTES: i32 = 60;

/// Media types a browser may show inline. Everything else is only ever downloaded: an HTML or SVG
/// file is code, and the owner's session must never run code an agent wrote.
const INLINE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Debug, Clone, FromRow)]
pub struct AttachmentRow {
    pub id: i64,
    pub task_id: String,
    pub kind: String,
    pub title: String,
    pub url: Option<String>,
    pub file_name: Option<String>,
    pub mime: Option<String>,
    pub size: Option<i64>,
    pub sha256: Option<String>,
    pub complete: bool,
    pub added_by: String,
    pub created_at: DateTime<Utc>,
}

const COLUMNS: &str =
    "id, task_id, kind, title, url, file_name, mime, size, sha256, complete, added_by, created_at";

impl AttachmentRow {
    /// Whether a browser may display it rather than download it.
    pub fn inline(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(|m| INLINE_TYPES.contains(&m))
    }
}

/// `A-12` or `12`, as a person or an agent writes it.
pub fn parse_id(s: &str) -> Result<i64> {
    let s = s.trim();
    let n = s
        .strip_prefix("A-")
        .or_else(|| s.strip_prefix("a-"))
        .unwrap_or(s);
    n.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .with_context(|| format!("{s:?} is not an attachment id (A-12)"))
}

/// A title as it will be shown: trimmed, one line, not empty, not a novel.
pub fn check_title(raw: &str) -> Result<String> {
    let t = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    ensure!(
        !t.is_empty(),
        "say what it is: a title is how it is found later"
    );
    ensure!(
        t.chars().count() <= 200,
        "a title is at most 200 characters"
    );
    Ok(t)
}

/// A link worth keeping: http(s), one token, not absurdly long.
pub fn check_url(raw: &str) -> Result<String> {
    let u = raw.trim();
    ensure!(
        u.starts_with("https://") || u.starts_with("http://"),
        "{u:?} is not a link: it must start with https:// or http://"
    );
    ensure!(u.len() <= 2000, "that link is longer than 2000 characters");
    ensure!(
        !u.chars().any(|c| c.is_whitespace() || c.is_control()),
        "a link cannot contain spaces"
    );
    Ok(u.to_owned())
}

/// A file's name as it will be offered for download: the last path component only, printable,
/// and not something that looks like a secrets file.
pub fn check_file_name(raw: &str) -> Result<String> {
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    ensure!(
        !name.is_empty() && name != "." && name != "..",
        "the file needs a name"
    );
    ensure!(
        name.chars().count() <= 200,
        "a file name is at most 200 characters"
    );
    ensure!(
        !name.chars().any(|c| c.is_control() || c == '"'),
        "{name:?} has characters a file name cannot carry"
    );
    ensure!(
        !is_env_file(&name),
        "{name} looks like a secrets file; attach what the work produced, never credentials"
    );
    Ok(name)
}

/// `.env`, `.env.local`, `prod.env`: the shapes secrets are kept in.
pub fn is_env_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == ".env" || n.starts_with(".env.") || n.ends_with(".env")
}

/// Every attachment of one task that is visible (uploads still in progress are not), oldest
/// first.
pub async fn of_task(o: &Orchestrator, task_id: &str) -> Result<Vec<AttachmentRow>> {
    Ok(sqlx::query_as(&format!(
        "select {COLUMNS} from task_attachments where task_id = $1 and complete order by id"
    ))
    .bind(task_id)
    .fetch_all(&o.db)
    .await?)
}

/// The attachments of many tasks at once, for a page that shows a lot of them.
pub async fn of_tasks(o: &Orchestrator, ids: &[String]) -> Result<Vec<AttachmentRow>> {
    Ok(sqlx::query_as(&format!(
        "select {COLUMNS} from task_attachments where task_id = any($1) and complete order by id"
    ))
    .bind(ids)
    .fetch_all(&o.db)
    .await?)
}

/// One visible attachment.
pub async fn get(o: &Orchestrator, id: i64) -> Result<Option<AttachmentRow>> {
    Ok(sqlx::query_as(&format!(
        "select {COLUMNS} from task_attachments where id = $1 and complete"
    ))
    .bind(id)
    .fetch_optional(&o.db)
    .await?)
}

pub fn json(a: &AttachmentRow) -> Value {
    json!({
        "id": a.id,
        "ref": format!("A-{}", a.id),
        "kind": a.kind,
        "title": a.title,
        "url": a.url,
        "file_name": a.file_name,
        "mime": a.mime,
        "size": a.size,
        "sha256": a.sha256,
        "added_by": a.added_by,
        "created_at": a.created_at,
        "download": (a.kind == "file").then(|| format!("/api/attachments/{}/file", a.id)),
    })
}

/// One line for an agent or the brain.
pub fn describe(a: &AttachmentRow) -> String {
    match (&a.url, &a.file_name) {
        (Some(u), _) => format!("A-{} {:?}: {u}", a.id, a.title),
        (None, Some(f)) => format!(
            "A-{} {:?}: file {f} ({})",
            a.id,
            a.title,
            human_size(a.size.unwrap_or(0))
        ),
        _ => format!("A-{} {:?}", a.id, a.title),
    }
}

pub fn human_size(n: i64) -> String {
    const KIB: f64 = 1024.0;
    let n = n as f64;
    if n < KIB {
        format!("{n} B")
    } else if n < KIB * KIB {
        format!("{:.1} KB", n / KIB)
    } else {
        format!("{:.1} MB", n / KIB / KIB)
    }
}

async fn task(o: &Orchestrator, id: &str) -> Result<TaskRow> {
    o.task(id).await?.with_context(|| format!("no task {id}"))
}

/// The secret values a file or link attached to `task` must not contain: its project's, and
/// those of the project the uploader works in (the brain attaching to another project's task
/// holds its own).
async fn forbidden_values(
    o: &Orchestrator,
    task: &TaskRow,
    uploader_project: Option<&str>,
) -> Result<Vec<Vec<u8>>> {
    let mut values = crate::tasks::project_secret_values(o, &task.project).await?;
    if let Some(p) = uploader_project.filter(|p| *p != task.project) {
        values.extend(crate::tasks::project_secret_values(o, p).await?);
    }
    Ok(values
        .into_iter()
        .filter(|v| v.len() >= fleet_proto::redact::MIN_REDACT_LEN)
        .map(String::into_bytes)
        .collect())
}

/// Attaches a link with a title. The same link attached again keeps one row and takes the newer
/// title, which is what an agent retrying a step (or correcting a title) needs.
pub async fn add_url(
    o: &Orchestrator,
    task_id: &str,
    url: &str,
    title: &str,
    added_by: &str,
    uploader_project: Option<&str>,
) -> Result<AttachmentRow> {
    let task = task(o, task_id).await?;
    let url = check_url(url)?;
    let title = check_title(title)?;
    let secrets = forbidden_values(o, &task, uploader_project).await?;
    if secrets
        .iter()
        .any(|s| memchr::memmem::find(url.as_bytes(), s).is_some())
    {
        bail!("that link contains one of the project's secret values; it was not attached");
    }
    // One row per link and task, however many times (or however concurrently) it is attached:
    // the unique index decides, and `xmax = 0` tells a fresh row from an updated one.
    let (id, fresh): (i64, bool) = sqlx::query_as(
        "insert into task_attachments (task_id, kind, title, url, complete, added_by)
         values ($1, 'url', $2, $3, true, $4)
         on conflict (task_id, url) where kind = 'url' do update set title = excluded.title
         returning id, (xmax = 0)",
    )
    .bind(&task.id)
    .bind(&title)
    .bind(&url)
    .bind(added_by)
    .fetch_one(&o.db)
    .await?;
    let row = get(o, id).await?.context("the attachment vanished")?;
    o.audit(
        added_by,
        "task.attach",
        json!({"task": task.id, "attachment": id, "kind": "url", "title": title, "url": url}),
    )
    .await;
    if fresh {
        announce(o, &task, &row).await;
    }
    Ok(row)
}

/// Starts a file upload and returns its id. Nothing is visible until [`finish`].
#[allow(clippy::too_many_arguments)]
pub async fn begin(
    o: &Orchestrator,
    task_id: &str,
    title: &str,
    file_name: &str,
    size: i64,
    sha256: &str,
    added_by: &str,
    uploader_project: Option<&str>,
) -> Result<i64> {
    let task = task(o, task_id).await?;
    let title = check_title(title)?;
    let file_name = check_file_name(file_name)?;
    // The name is shown in Telegram, the audit log and a download header, so it is held to the
    // same rule as the bytes: no secret value in it.
    if forbidden_values(o, &task, uploader_project)
        .await?
        .iter()
        .any(|s| memchr::memmem::find(file_name.as_bytes(), s).is_some())
    {
        bail!("that file name contains one of the project's secret values; choose another");
    }
    let max = o.cfg.attachment_max_bytes;
    ensure!(size > 0, "the file is empty");
    ensure!(
        size <= max,
        "{file_name} is {}, more than the {} a file may be (FLEET_ATTACHMENT_MAX_MB)",
        human_size(size),
        human_size(max)
    );
    let sha256 = sha256.trim().to_ascii_lowercase();
    ensure!(
        sha256.len() == 64 && sha256.chars().all(|c| c.is_ascii_hexdigit()),
        "sha256 must be the file's SHA-256 as 64 hex digits"
    );
    Ok(sqlx::query_scalar(
        "insert into task_attachments (task_id, kind, title, file_name, size, sha256, complete, added_by)
         values ($1, 'file', $2, $3, $4, $5, false, $6) returning id",
    )
    .bind(&task.id)
    .bind(&title)
    .bind(&file_name)
    .bind(size)
    .bind(&sha256)
    .bind(added_by)
    .fetch_one(&o.db)
    .await?)
}

/// An unfinished upload, as only the one who began it may continue it.
async fn upload(
    o: &Orchestrator,
    upload_id: i64,
    task_id: &str,
    added_by: &str,
) -> Result<AttachmentRow> {
    let row: Option<AttachmentRow> = sqlx::query_as(&format!(
        "select {COLUMNS} from task_attachments where id = $1 and kind = 'file' and not complete"
    ))
    .bind(upload_id)
    .fetch_optional(&o.db)
    .await?;
    let row = row.with_context(|| {
        format!("no upload {upload_id} in progress (finished, abandoned, or never begun)")
    })?;
    // The task is compared by its real id: `task_id` may be the key it was named by.
    let task = task(o, task_id).await?;
    ensure!(
        row.task_id == task.id && row.added_by == added_by,
        "upload {upload_id} belongs to someone else"
    );
    Ok(row)
}

fn chunks_of(size: i64) -> i64 {
    (size + CHUNK as i64 - 1) / CHUNK as i64
}

/// Stores one piece of an upload. Sending the same piece again replaces it, so a retry after a
/// lost reply is harmless.
///
/// The piece is written under a shared lock on the upload's row, taken after checking that it is
/// still unfinished: [`finish`] holds the exclusive one while it checks and publishes, so a piece
/// can never land (or be replaced) between the check and the upload becoming visible.
pub async fn chunk(
    o: &Orchestrator,
    upload_id: i64,
    task_id: &str,
    added_by: &str,
    idx: i64,
    data: &[u8],
) -> Result<()> {
    let row = upload(o, upload_id, task_id, added_by).await?;
    let n = chunks_of(row.size.unwrap_or(0));
    ensure!(
        (0..n).contains(&idx),
        "chunk {idx} is out of range: this upload has chunks 0..{}",
        n - 1
    );
    ensure!(
        !data.is_empty() && data.len() <= CHUNK,
        "a chunk carries 1 to {CHUNK} bytes, not {}",
        data.len()
    );
    let mut tx = o.db.begin().await?;
    let open: Option<i64> = sqlx::query_scalar(
        "select id from task_attachments where id = $1 and not complete for share",
    )
    .bind(upload_id)
    .fetch_optional(&mut *tx)
    .await?;
    ensure!(
        open.is_some(),
        "upload {upload_id} is already finished or was discarded"
    );
    sqlx::query(
        "insert into task_attachment_chunks (attachment_id, idx, data) values ($1, $2, $3)
         on conflict (attachment_id, idx) do update set data = excluded.data",
    )
    .bind(upload_id)
    .bind(idx as i32)
    .bind(data)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // Every piece is a sign of life, for the sweep of abandoned uploads.
    sqlx::query("update task_attachments set created_at = now() where id = $1 and not complete")
        .bind(upload_id)
        .execute(&o.db)
        .await?;
    Ok(())
}

/// Checks a finished upload and makes it visible.
///
/// The check and the publication happen under an exclusive lock on the upload's row, which no
/// piece can be written without (see [`chunk`]): what was hashed and scanned is exactly what is
/// published, and of two `finish` calls at once only one publishes. An upload that fails the
/// check (a missing piece, a wrong size or hash, a secret inside) is thrown away, so the next try
/// starts clean with `begin`; one that hits a passing database error is kept for a retry.
pub async fn finish(
    o: &Orchestrator,
    upload_id: i64,
    task_id: &str,
    added_by: &str,
    uploader_project: Option<&str>,
) -> Result<AttachmentRow> {
    let row = upload(o, upload_id, task_id, added_by).await?;
    let task = task(o, &row.task_id).await?;
    let mut tx = o.db.begin().await?;
    let open: Option<i64> = sqlx::query_scalar(
        "select id from task_attachments where id = $1 and not complete for update",
    )
    .bind(upload_id)
    .fetch_optional(&mut *tx)
    .await?;
    ensure!(
        open.is_some(),
        "upload {upload_id} is already finished or was discarded"
    );
    if let Some(refused) = verify(o, &row, &task, uploader_project).await? {
        drop(tx);
        discard(o, upload_id).await;
        bail!(refused);
    }
    let name = row.file_name.clone().unwrap_or_default();
    let mime = mime_guess::from_path(&name)
        .first()
        .map(|m| m.essence_str().to_owned())
        .unwrap_or_else(|| "application/octet-stream".into());
    sqlx::query(
        "update task_attachments set complete = true, mime = $2, created_at = now()
         where id = $1 and not complete",
    )
    .bind(upload_id)
    .bind(&mime)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let row = get(o, upload_id)
        .await?
        .context("the attachment vanished")?;
    o.audit(
        added_by,
        "task.attach",
        json!({"task": task.id, "attachment": upload_id, "kind": "file", "title": row.title,
               "file": name, "size": row.size, "sha256": row.sha256}),
    )
    .await;
    announce(o, &task, &row).await;
    Ok(row)
}

/// Reads an upload's pieces in order, one at a time, and checks that they are all there, add up
/// to the declared size, hash to the declared SHA-256 and carry no secret value — including one
/// that straddles two pieces. `Ok(Some(why))` is a refusal; `Err` is the database failing, which
/// says nothing about the file.
async fn verify(
    o: &Orchestrator,
    row: &AttachmentRow,
    task: &TaskRow,
    uploader_project: Option<&str>,
) -> Result<Option<String>> {
    let size = row.size.unwrap_or(0);
    let n = chunks_of(size);
    let have: i64 =
        sqlx::query_scalar("select count(*) from task_attachment_chunks where attachment_id = $1")
            .bind(row.id)
            .fetch_one(&o.db)
            .await?;
    if have != n {
        return Ok(Some(format!(
            "only {have} of {n} chunks arrived; the upload was discarded, start again"
        )));
    }
    let secrets = forbidden_values(o, task, uploader_project).await?;
    let mut scan = SecretScan::new(&secrets);
    let mut hash = Sha256::new();
    let mut total: i64 = 0;
    for idx in 0..n {
        let data: Vec<u8> = sqlx::query_scalar(
            "select data from task_attachment_chunks where attachment_id = $1 and idx = $2",
        )
        .bind(row.id)
        .bind(idx as i32)
        .fetch_one(&o.db)
        .await?;
        total += data.len() as i64;
        hash.update(&data);
        if scan.feed(&data) {
            return Ok(Some(format!(
                "{} contains one of the project's secret values; it was not attached. \
                 Attach what the work produced, never credentials.",
                row.file_name.as_deref().unwrap_or("the file")
            )));
        }
    }
    if total != size {
        return Ok(Some(format!(
            "the chunks add up to {total} bytes, not the {size} declared; the upload was discarded"
        )));
    }
    let got = data_encoding::HEXLOWER.encode(&hash.finalize());
    if Some(got.as_str()) != row.sha256.as_deref() {
        return Ok(Some(
            "the bytes that arrived do not hash to the declared SHA-256; the upload was \
             discarded, start again"
                .into(),
        ));
    }
    Ok(None)
}

/// Finds any of a set of byte strings in a stream fed piece by piece, without holding the whole
/// stream: each piece is searched together with the tail of the one before, as long as the
/// longest needle less one byte, so a match across a boundary is still seen.
pub struct SecretScan<'a> {
    needles: &'a [Vec<u8>],
    tail: Vec<u8>,
    keep: usize,
}

impl<'a> SecretScan<'a> {
    pub fn new(needles: &'a [Vec<u8>]) -> Self {
        let keep = needles
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(1)
            .saturating_sub(1);
        Self {
            needles,
            tail: Vec::new(),
            keep,
        }
    }

    /// Whether a needle has been seen so far.
    pub fn feed(&mut self, data: &[u8]) -> bool {
        if self.needles.is_empty() {
            return false;
        }
        let mut window = std::mem::take(&mut self.tail);
        window.extend_from_slice(data);
        let found = self
            .needles
            .iter()
            .any(|n| memchr::memmem::find(&window, n).is_some());
        let start = window.len().saturating_sub(self.keep);
        self.tail = window.split_off(start);
        found
    }
}

async fn discard(o: &Orchestrator, upload_id: i64) {
    let _ = sqlx::query("delete from task_attachments where id = $1 and not complete")
        .bind(upload_id)
        .execute(&o.db)
        .await;
}

/// Removes a visible attachment. Returns whether there was one on that task.
///
/// `only_by` limits it to what that actor attached: an agent takes back its own attachments, not
/// the owner's runbook on the same task. The owner and the brain pass `None`.
pub async fn remove(
    o: &Orchestrator,
    task_id: &str,
    id: i64,
    actor: &str,
    only_by: Option<&str>,
) -> Result<bool> {
    let task = task(o, task_id).await?;
    let gone: Option<String> = sqlx::query_scalar(
        "delete from task_attachments
         where id = $1 and task_id = $2 and complete and ($3::text is null or added_by = $3)
         returning title",
    )
    .bind(id)
    .bind(&task.id)
    .bind(only_by)
    .fetch_optional(&o.db)
    .await?;
    if let Some(title) = &gone {
        o.audit(
            actor,
            "task.detach",
            json!({"task": task.id, "attachment": id, "title": title}),
        )
        .await;
    }
    Ok(gone.is_some())
}

/// The number of pieces a visible file is stored in, for streaming it out.
pub fn chunk_count(a: &AttachmentRow) -> i64 {
    chunks_of(a.size.unwrap_or(0))
}

/// One stored piece of a file.
pub async fn read_chunk(o: &Orchestrator, id: i64, idx: i64) -> Result<Vec<u8>> {
    Ok(sqlx::query_scalar(
        "select data from task_attachment_chunks where attachment_id = $1 and idx = $2",
    )
    .bind(id)
    .bind(idx as i32)
    .fetch_one(&o.db)
    .await?)
}

/// Throws away uploads that stopped arriving.
pub async fn sweep_stale(o: &Orchestrator) -> Result<u64> {
    Ok(sqlx::query(
        "delete from task_attachments
         where not complete and created_at < now() - make_interval(mins => $1::int)",
    )
    .bind(STALE_UPLOAD_MINUTES)
    .execute(&o.db)
    .await?
    .rows_affected())
}

/// Says so in the task's topic, while the work is still going on there. A finished task's topic
/// is closed (or gone), and the web page shows the attachment either way, so nothing is posted
/// for one. Best effort: the attachment is there whatever Telegram does.
async fn announce(o: &Orchestrator, task: &TaskRow, a: &AttachmentRow) {
    if task.topic_id.is_none() || crate::model::status::is_done(&task.status) {
        return;
    }
    let title = esc(&clip(&a.title, 200));
    let html = match (&a.url, &a.file_name) {
        (Some(u), _) => format!("📎 <a href=\"{}\">{title}</a>", esc(u)),
        (None, Some(f)) => {
            let page = crate::web::magic_link(o, &format!("/tasks/{}", task.id))
                .await
                .ok();
            let what = format!(
                "📎 <b>{title}</b> — {} ({})",
                esc(f),
                human_size(a.size.unwrap_or(0))
            );
            match page {
                Some(link) => format!("{what}\n<a href=\"{}\">On the task's page</a>", esc(&link)),
                None => what,
            }
        }
        _ => return,
    };
    o.post_task(task, &html, Notify::Silent).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_links_and_names_are_checked() {
        assert_eq!(
            check_title("  Maestro \n prompt comparison ").unwrap(),
            "Maestro prompt comparison"
        );
        assert!(check_title("   ").is_err());
        assert!(check_title(&"x".repeat(201)).is_err());
        let u = "https://claude.ai/artifact/Q5GfaHwvtR471EHGo8ZVAB";
        assert_eq!(check_url(u).unwrap(), u);
        assert!(check_url("claude.ai/artifact/x").is_err());
        assert!(check_url("javascript:alert(1)").is_err());
        assert!(check_url("https://a b").is_err());
        assert_eq!(
            check_file_name("/tmp/out/0021_x.sql").unwrap(),
            "0021_x.sql"
        );
        assert_eq!(check_file_name(r"C:\work\logo.png").unwrap(), "logo.png");
        for bad in [
            "",
            "dir/",
            "..",
            ".env",
            ".env.production",
            "prod.env",
            "a\"b",
        ] {
            assert!(check_file_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn attachment_ids_read_either_way() {
        assert_eq!(parse_id("A-12").unwrap(), 12);
        assert_eq!(parse_id(" 7 ").unwrap(), 7);
        assert!(parse_id("F-12").is_err());
        assert!(parse_id("0").is_err());
    }

    #[test]
    fn a_secret_split_across_two_pieces_is_still_found() {
        let needles = vec![b"hunter2-secret".to_vec(), b"zz".to_vec()];
        let mut s = SecretScan::new(&needles);
        assert!(!s.feed(b"some harmless bytes ... hunter2-"));
        assert!(s.feed(b"secret and more"));
        let mut s = SecretScan::new(&needles);
        for piece in [&b"aaaz"[..], b"b", b"z"] {
            // "z" then "b" then "z": never "zz".
            assert!(!s.feed(piece));
        }
        assert!(s.feed(b"z"), "z|z across pieces");
        let none: Vec<Vec<u8>> = vec![];
        assert!(!SecretScan::new(&none).feed(b"anything"));
    }

    #[test]
    fn only_raster_images_are_shown_inline() {
        let mut a = AttachmentRow {
            id: 1,
            task_id: "t".into(),
            kind: "file".into(),
            title: "x".into(),
            url: None,
            file_name: Some("x.png".into()),
            mime: Some("image/png".into()),
            size: Some(1),
            sha256: None,
            complete: true,
            added_by: "a".into(),
            created_at: Utc::now(),
        };
        assert!(a.inline());
        for m in ["image/svg+xml", "text/html", "application/octet-stream"] {
            a.mime = Some(m.into());
            assert!(!a.inline(), "{m}");
        }
    }
}
