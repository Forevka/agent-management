//! Finding a task again: one filter shared by the web UI's task list and the brain's
//! `list_tasks`, so both answer the same question the same way.
//!
//! A query is split into words and **every word must match somewhere**: the id, the `F-41` key,
//! the title, the brief, the agent's closing summary, the status, the agent (id or name), a link
//! (`BIOINS-5068`, `!940`, its URL) or an attachment (title, URL, file name). "maestro
//! comparison" finds the task titled "Maestro prompts" whose artifact is called "comparison".
//!
//! Archived tasks (done, with `archived_at` set) are hidden from a plain list, which is what
//! archiving is for, and included as soon as somebody is looking for something: a query,
//! `archived` asked for explicitly, or the status `archived`, which is not a status a task has but
//! a filter for exactly the archived ones (the word `archived` in a query matches them the same
//! way). A word that matches nothing (a null anywhere included) excludes the task:
//! the whole test is wrapped in `coalesce(.., false)` so that an unkeyed task's null key cannot
//! make `not (..)` null and let everything through.
//! The words reach SQL as a bound array and are matched with `ilike`
//! after escaping its wildcards: user text is never formatted into the query.

use anyhow::Result;

use crate::{core::Orchestrator, model::TaskRow};

/// What to list. Absent fields do not filter.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub project: Option<String>,
    pub status: Option<String>,
    pub agent: Option<String>,
    pub q: Option<String>,
    /// Include the fleet's own standing sessions.
    pub builtin: bool,
    /// Include archived tasks even without a query.
    pub archived: bool,
    pub limit: i64,
    pub offset: i64,
}

/// The query's words, each escaped for `ilike` (`%`, `_` and `\` match themselves). At most 12:
/// a search is a few words, and the cost grows with each.
pub fn words(q: &str) -> Vec<String> {
    q.split_whitespace()
        .take(12)
        .map(|w| {
            let mut e = String::with_capacity(w.len());
            for c in w.chars() {
                if matches!(c, '%' | '_' | '\\') {
                    e.push('\\');
                }
                e.push(c);
            }
            e
        })
        .collect()
}

/// `$1` project, `$2` status, `$3` agent, `$4` words, `$5` builtin, `$6` archived, `$7` the
/// builtin task ids, `$8` the task-key prefix.
const WHERE: &str = "where ($1::text is null or tasks.project = $1)
   and ($2::text is null
        or ($2 = 'archived' and tasks.status = 'done' and tasks.archived_at is not null)
        or tasks.status = $2)
   and ($3::text is null or tasks.agent_id = $3)
   and ($5 or tasks.id <> all($7))
   and (tasks.archived_at is null or tasks.status <> 'done'
        or $6 or $2::text = 'archived' or cardinality($4::text[]) > 0)
   and not exists (
     select 1 from unnest($4::text[]) w
     where not coalesce((
          tasks.id ilike '%' || w || '%'
       or tasks.title ilike '%' || w || '%'
       or tasks.brief ilike '%' || w || '%'
       or coalesce(tasks.summary, '') ilike '%' || w || '%'
       or tasks.status ilike w
       or (w ilike 'archived' and tasks.status = 'done' and tasks.archived_at is not null)
       or upper(w) = upper($8) || '-' || tasks.key_no
       or exists (select 1 from agents a where a.id = tasks.agent_id
                  and (a.id ilike '%' || w || '%' or a.name ilike '%' || w || '%'))
       or exists (select 1 from task_links l where l.task_id = tasks.id
                  and (l.ref ilike '%' || w || '%' or coalesce(l.url, '') ilike '%' || w || '%'))
       or exists (select 1 from task_attachments x where x.task_id = tasks.id and x.complete
                  and (x.title ilike '%' || w || '%'
                       or coalesce(x.url, '') ilike '%' || w || '%'
                       or coalesce(x.file_name, '') ilike '%' || w || '%'))
     ), false))";

/// The matching tasks, most recently active first, and how many there are in all.
pub async fn tasks(o: &Orchestrator, f: &Filter) -> Result<(Vec<TaskRow>, i64)> {
    let words = f.q.as_deref().map(words).unwrap_or_default();
    let rows: Vec<TaskRow> = sqlx::query_as(&format!(
        "select * from tasks {WHERE} order by updated_at desc limit $9 offset $10"
    ))
    .bind(&f.project)
    .bind(&f.status)
    .bind(&f.agent)
    .bind(&words)
    .bind(f.builtin)
    .bind(f.archived)
    .bind(crate::model::BUILTIN_TASKS)
    .bind(crate::model::TASK_KEY_PREFIX)
    .bind(f.limit)
    .bind(f.offset)
    .fetch_all(&o.db)
    .await?;
    let total: i64 = sqlx::query_scalar(&format!("select count(*) from tasks {WHERE}"))
        .bind(&f.project)
        .bind(&f.status)
        .bind(&f.agent)
        .bind(&words)
        .bind(f.builtin)
        .bind(f.archived)
        .bind(crate::model::BUILTIN_TASKS)
        .bind(crate::model::TASK_KEY_PREFIX)
        .fetch_one(&o.db)
        .await?;
    Ok((rows, total))
}

#[cfg(test)]
mod tests {
    use super::words;

    #[test]
    fn words_are_split_and_their_wildcards_escaped() {
        assert_eq!(words("  Maestro   F-7 "), ["Maestro", "F-7"]);
        assert_eq!(words("100% a_b c\\d"), ["100\\%", "a\\_b", "c\\\\d"]);
        assert!(words("   ").is_empty());
        assert_eq!(words(&"w ".repeat(40)).len(), 12);
    }
}
