//! The owner's operating rules: how the fleet acts in their name.
//!
//! Approving a colleague's MR, merging, posting a comment, moving a ticket, replying as the
//! owner: rules about these used to live wherever someone wrote them down — the brain's memory,
//! the shared bank, a habit's brief — and nothing tied a copy back to the rule it came from or
//! noticed when a later rule replaced it (retro P-8: a habit kept "approve if nothing is
//! blocking" after the owner had said "never approve"). So a rule is a record here, `OR-n`, and
//! everything that acts on one cites it instead of keeping its own copy:
//!
//! - a rule is written only by an approved `admin_change` (`RuleAdd`, `RuleRemove`), because
//!   its current text reaches every standing request of the habits that cite it — a rule a feed
//!   message or an agent could plant would change what a habit does without the owner;
//! - a rule is never edited, only replaced: the new one names the one it `supersedes`, and the
//!   old one stays readable as history;
//! - a `do` habit whose brief states what to do on an outward action must cite a rule
//!   ([`outward_actions`] is the check, at proposal time), and its standing request carries the
//!   rule's current text after the brief;
//! - replacing or removing a rule pauses every habit that cites it, so the owner reads its brief
//!   against the new rule before it acts again.
//!
//! Triage preferences stay secretary rules (`secretary_rules`): those only make triage quieter
//! or louder and never act in the owner's name.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    chat::{From, Notify, clip, esc},
    core::Orchestrator,
    habits,
    model::OwnerRuleRow,
    topics,
};

/// At most this many rules hold at once, so they stay something the owner can read in one
/// screen, like the habits.
pub const MAX_RULES: i64 = 40;

/// The most rules one habit may cite.
pub const MAX_CITED: usize = 5;

/// Longest rule text: one line the owner can read on a card.
pub const MAX_TEXT: usize = 300;

/// The outward actions a brief can state policy on, each with the stems that name it, in the
/// languages the owner writes in. A Latin stem matches the start of a word ("approved"); a
/// Cyrillic one anywhere in it, because Ukrainian and Russian make the perfective with a prefix
/// ("заапрувь", "прокоментуй"); `=word` matches only that whole word ("post", not "postgres").
/// Only ever used to *require* a citation, so a false positive costs the brain one `--rule`, and
/// a miss is no worse than before this check existed.
const OUTWARD: &[(&str, &[&str])] = &[
    (
        "approve",
        &[
            "approv",
            "lgtm",
            "апрув",
            "аппрув",
            "затверд",
            "схвал",
            "одобр",
        ],
    ),
    ("merge", &["merg", "мерж", "мердж"]),
    ("transition", &["transition", "move", "перевест", "перевед"]),
    ("comment", &["comment", "комент", "коммент"]),
    (
        "reply",
        &[
            "reply",
            "replie",
            "respond",
            "відпиш",
            "відповіс",
            "отпиш",
            "ответь",
            "ответит",
        ],
    ),
    ("send", &["send", "надісл", "надішл", "відправ", "отправ"]),
    ("publish", &["publish", "опублік", "опублик"]),
    (
        "post",
        &["=post", "=posts", "=posted", "=posting", "запост", "постни"],
    ),
];

/// Words a stem would catch that name no action: asking about an approval is not giving one.
const NOT_OUTWARD: &[&str] = &["approval"];

/// "Send me a summary" and "post it to me" go to the owner, not out in their name.
const TO_THE_OWNER: &[&str] = &["me", "мені", "мне"];
const TO_THE_OWNER_ACTIONS: &[&str] = &["send", "post"];

fn names(stem: &str, word: &str) -> bool {
    if let Some(exact) = stem.strip_prefix('=') {
        word == exact
    } else if stem.is_ascii() {
        word.starts_with(stem)
    } else {
        word.contains(stem)
    }
}

/// The outward actions `text` states something about, in a fixed order, each once.
pub fn outward_actions(text: &str) -> Vec<&'static str> {
    let text = text.to_lowercase();
    let words: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let mut found = vec![];
    for (action, stems) in OUTWARD {
        let named = words.iter().enumerate().any(|(i, w)| {
            if NOT_OUTWARD.iter().any(|n| w.starts_with(n)) || !stems.iter().any(|s| names(s, w)) {
                return false;
            }
            !(TO_THE_OWNER_ACTIONS.contains(action)
                && words[i + 1..]
                    .iter()
                    .take(4)
                    .any(|n| TO_THE_OWNER.contains(n)))
        });
        if named {
            found.push(*action);
        }
    }
    found
}

/// A rule's text as written: one line, no control characters, not too long.
pub fn check_text(text: &str) -> Result<String> {
    let text = text.trim().to_owned();
    ensure!(!text.is_empty(), "the rule's text is empty");
    ensure!(
        !text.chars().any(char::is_control),
        "a rule is one line, without control characters"
    );
    ensure!(
        text.chars().count() <= MAX_TEXT,
        "a rule is at most {MAX_TEXT} characters; say it in one line"
    );
    Ok(text)
}

pub async fn get(o: &Orchestrator, id: i64) -> Result<OwnerRuleRow> {
    let row: Option<OwnerRuleRow> = sqlx::query_as("select * from owner_rules where id = $1")
        .bind(id)
        .fetch_optional(&o.db)
        .await?;
    row.with_context(|| format!("no owner rule OR-{id}"))
}

/// The rule that replaced `id`, if one did.
pub async fn replaced_by(o: &Orchestrator, id: i64) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("select id from owner_rules where supersedes = $1")
            .bind(id)
            .fetch_optional(&o.db)
            .await?,
    )
}

/// A rule that may be cited or replaced: it exists and still holds. The error says what
/// replaced it, so the brain can cite that one instead.
pub async fn live(o: &Orchestrator, id: i64) -> Result<OwnerRuleRow> {
    let row = get(o, id).await?;
    if !row.active {
        match replaced_by(o, id).await? {
            Some(new) => bail!("OR-{id} was replaced by OR-{new}; cite or replace that one"),
            None => bail!("OR-{id} was removed and no longer holds"),
        }
    }
    Ok(row)
}

/// The rules `ids` names, in id order (missing ones left out).
pub async fn cited(o: &Orchestrator, ids: &[i64]) -> Result<Vec<OwnerRuleRow>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    Ok(
        sqlx::query_as("select * from owner_rules where id = any($1) order by id")
            .bind(ids)
            .fetch_all(&o.db)
            .await?,
    )
}

/// The habits that cite rule `id`: (habit, active, what it is).
pub async fn citing(o: &Orchestrator, id: i64) -> Result<Vec<(i64, bool, String)>> {
    let rows: Vec<crate::model::HabitRow> =
        sqlx::query_as("select * from habits where $1 = any(rules) order by id")
            .bind(id)
            .fetch_all(&o.db)
            .await?;
    Ok(rows
        .iter()
        .map(|h| (h.id, h.active, habits::describe(h)))
        .collect())
}

/// What a habit paused in the same transaction as a rule change hears afterwards: its card,
/// the audit and the brain, as when the owner pauses one. The pause itself was already written,
/// so a crash here costs a stale card, never a habit acting on a brief written for an old rule.
async fn announce(o: &Arc<Orchestrator>, ids: &[(i64, bool)], why: &str) -> Vec<i64> {
    let mut paused = vec![];
    for &(id, was_active) in ids {
        if !was_active {
            continue;
        }
        match sqlx::query_as::<_, crate::model::HabitRow>("select * from habits where id = $1")
            .bind(id)
            .fetch_optional(&o.db)
            .await
        {
            Ok(Some(row)) => habits::paused(o, &row, why).await,
            Ok(None) => {}
            Err(e) => tracing::warn!("reading habit H-{id} after a rule change failed: {e:#}"),
        }
        paused.push(id);
    }
    paused
}

/// Pauses, inside `tx`, every habit that cites rule `old` (moving the citation to `new` when
/// there is one): (habit, whether it was active). One statement, so no habit is left both
/// active and citing a rule that changed under it.
async fn pause_citing(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    old: i64,
    new: Option<i64>,
    why: &str,
) -> Result<Vec<(i64, bool)>> {
    Ok(sqlx::query_as(
        "with c as (select id, active from habits where $1 = any(rules) for update)
         update habits h
            set rules = case when $2::bigint is null then h.rules
                             else array_replace(h.rules, $1, $2) end,
                active = false,
                paused_why = case when c.active then $3 else h.paused_why end
           from c where h.id = c.id
         returning h.id, c.active",
    )
    .bind(old)
    .bind(new)
    .bind(clip(why, 200))
    .fetch_all(&mut **tx)
    .await?)
}

fn habit_list(ids: &[i64]) -> String {
    ids.iter()
        .map(|id| format!("H-{id}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Records an approved rule, replacing `supersedes` if given. Every habit that cited the old
/// rule now cites the new one and is paused, so it does not act on a brief written for the old
/// rule until the owner has read it again. Returns what happened, for the owner.
pub async fn add(
    o: &Arc<Orchestrator>,
    actor: &str,
    text: &str,
    because: &str,
    supersedes: Option<i64>,
) -> Result<String> {
    let mut tx = o.db.begin().await?;
    if let Some(old) = supersedes {
        // Only a rule that still holds can be replaced; the unique `supersedes` column keeps two
        // approvals of rival replacements from both landing.
        let ended: Option<i64> = sqlx::query_scalar(
            "update owner_rules set active = false, ended_at = now()
              where id = $1 and active returning id",
        )
        .bind(old)
        .fetch_optional(&mut *tx)
        .await?;
        ensure!(
            ended.is_some(),
            "OR-{old} no longer holds; nothing to replace"
        );
    }
    let row: OwnerRuleRow = sqlx::query_as(
        "insert into owner_rules (text, because, supersedes) values ($1, $2, $3) returning *",
    )
    .bind(text)
    .bind(because)
    .bind(supersedes)
    .fetch_one(&mut *tx)
    .await?;
    let new = row.id;
    let why = supersedes.map(|old| {
        format!("the rule it follows, OR-{old}, was replaced by OR-{new}: check its brief, then resume it")
    });
    let moved = match (supersedes, why.as_deref()) {
        (Some(old), Some(why)) => pause_citing(&mut tx, old, Some(new), why).await?,
        _ => vec![],
    };
    tx.commit().await?;
    o.audit(
        actor,
        if supersedes.is_some() {
            "rule.supersede"
        } else {
            "rule.add"
        },
        json!({"rule": new, "supersedes": supersedes, "habits": moved.iter().map(|m| m.0).collect::<Vec<_>>()}),
    )
    .await;
    let mut done = format!("owner rule OR-{new} recorded");
    let mut note = None;
    if let (Some(old), Some(why)) = (supersedes, why.as_deref()) {
        let paused = announce(o, &moved, why).await;
        done = format!("owner rule OR-{new} replaces OR-{old}");
        if !paused.is_empty() {
            done.push_str(&format!(
                "; paused {} until you check their briefs",
                habit_list(&paused)
            ));
            note = Some(format!(
                "Paused until you check their briefs against it: {}.",
                habit_list(&paused)
            ));
        }
        if let Ok(old_row) = get(o, old).await {
            show(o, &old_row, Some(new), None).await;
        }
        o.tell_brain(format!(
            "[Owner rule OR-{old} replaced by OR-{new}] Cite OR-{new} from now on, and stop following \
             any copy of OR-{old} you hold in memory or in a brief.{}",
            if paused.is_empty() {
                String::new()
            } else {
                format!(
                    " Paused because they cite it: {}. Their briefs may need a new habit that \
                     follows OR-{new}; propose one only if the owner asks.",
                    habit_list(&paused)
                )
            }
        ));
    }
    show(o, &row, None, note.as_deref()).await;
    Ok(done)
}

/// Ends an approved rule without a replacement, pausing the habits that cite it.
pub async fn remove(o: &Arc<Orchestrator>, actor: &str, id: i64) -> Result<String> {
    let mut tx = o.db.begin().await?;
    let row: Option<OwnerRuleRow> = sqlx::query_as(
        "update owner_rules set active = false, ended_at = now()
          where id = $1 and active returning *",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let row = row.with_context(|| format!("OR-{id} no longer holds"))?;
    let why = format!("the rule it follows, OR-{id}, was removed: check its brief, then resume it");
    let ids = pause_citing(&mut tx, id, None, &why).await?;
    tx.commit().await?;
    o.audit(
        actor,
        "rule.remove",
        json!({"rule": id, "habits": ids.iter().map(|m| m.0).collect::<Vec<_>>()}),
    )
    .await;
    let paused = announce(o, &ids, &why).await;
    show(o, &row, None, None).await;
    o.tell_brain(format!(
        "[Owner rule OR-{id} removed] It no longer holds; stop following any copy of it.{}",
        if paused.is_empty() {
            String::new()
        } else {
            format!(" Paused because they cite it: {}.", habit_list(&paused))
        }
    ));
    let mut done = format!("owner rule OR-{id} removed");
    if !paused.is_empty() {
        done.push_str(&format!("; paused {}", habit_list(&paused)));
    }
    Ok(done)
}

/// Writes or rewrites a rule's card in the Rules topic, where the habits' cards are too. A card
/// has no buttons: changing a rule is a proposal the owner approves, like any change.
async fn show(
    o: &Arc<Orchestrator>,
    rule: &OwnerRuleRow,
    replaced_by: Option<i64>,
    note: Option<&str>,
) {
    let head = format!("<b>Owner rule OR-{}</b> {}", rule.id, esc(&rule.text));
    let mut html = if rule.active {
        format!("📏 {head}")
    } else {
        match replaced_by {
            Some(new) => format!("<s>📏 {head}</s>\nReplaced by OR-{new}."),
            None => format!("<s>📏 {head}</s>\nRemoved."),
        }
    };
    html.push_str(&format!(
        "\n<i>Your words, as quoted when it was proposed: {}</i>",
        esc(&clip(&rule.because, 300))
    ));
    if let Some(old) = rule.supersedes {
        html.push_str(&format!("\nReplaces OR-{old}."));
    }
    if let Some(note) = note {
        html.push_str(&format!("\n{}", esc(note)));
    }
    match rule.tg_msg {
        Some(msg) => {
            if let Err(e) = o.chat.edit(From::Orchestrator, msg, &html, vec![]).await {
                tracing::warn!("updating rule OR-{}'s card failed: {e:#}", rule.id);
            }
        }
        None => {
            let topic = match topics::rules(o).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("no Rules topic for rule OR-{}: {e:#}", rule.id);
                    return;
                }
            };
            match o
                .chat
                .send(
                    From::Orchestrator,
                    Some(topic),
                    &html,
                    vec![],
                    None,
                    Notify::Silent,
                )
                .await
            {
                Ok(msg) => {
                    let _ = sqlx::query("update owner_rules set tg_msg = $2 where id = $1")
                        .bind(rule.id)
                        .bind(msg)
                        .execute(&o.db)
                        .await;
                }
                Err(e) => tracing::warn!("posting rule OR-{}'s card failed: {e:#}", rule.id),
            }
        }
    }
}

/// `admin show rules`: the rules that hold, with the habits citing each, and the ones that
/// ended in the last 30 days with what replaced them — so the brain can tell a current rule
/// from a copy of an old one it remembers.
pub async fn json(o: &Orchestrator) -> Result<Value> {
    let rows: Vec<OwnerRuleRow> = sqlx::query_as(
        "select * from owner_rules where active or ended_at > now() - interval '30 days'
          order by id",
    )
    .fetch_all(&o.db)
    .await?;
    let mut holding = vec![];
    let mut ended = vec![];
    for r in rows {
        let habits: Vec<String> = citing(o, r.id)
            .await?
            .into_iter()
            .map(|(id, active, _)| {
                if active {
                    format!("H-{id}")
                } else {
                    format!("H-{id} (paused)")
                }
            })
            .collect();
        if r.active {
            holding.push(json!({
                "id": format!("OR-{}", r.id),
                "text": r.text,
                "because": r.because,
                "replaces": r.supersedes.map(|s| format!("OR-{s}")),
                "since": r.created_at,
                "cited_by": habits,
            }));
        } else {
            ended.push(json!({
                "id": format!("OR-{}", r.id),
                "text": r.text,
                "replaced_by": replaced_by(o, r.id).await?.map(|n| format!("OR-{n}")),
                "ended_at": r.ended_at,
            }));
        }
    }
    Ok(json!({
        "note": "The owner's operating rules: how the fleet acts in their name. Each was approved \
                 by the owner. A rule here wins over any copy of it in memory or in a brief; \
                 cite it by id (habit-add --rule OR-n, and in task briefs). To change one, propose \
                 `admin rule-add --supersedes OR-n`.",
        "rules": holding,
        "ended": ended,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_brief_that_acts_in_the_owners_name_is_noticed() {
        // H-1's brief, as the retrospective quoted it.
        let h1 = "…зроби ревʼю: коментарі inline в МР, approve якщо немає blocking/High…";
        assert_eq!(outward_actions(h1), vec!["approve", "comment"]);
        for (brief, actions) in [
            (
                "Merge it when green, then post a summary in #team",
                vec!["merge", "post"],
            ),
            ("апрувни МР і переведи тікет", vec!["approve", "transition"]),
            ("заапрувь і замерж", vec!["approve", "merge"]),
            ("прокоментуй МР", vec!["comment"]),
            ("Відпиши Ані, що п'ятниця підходить", vec!["reply"]),
            ("відправ Ані звіт", vec!["send"]),
            ("move the ticket to In Review", vec!["transition"]),
            ("LGTM it when the pipeline is green", vec!["approve"]),
            ("Approved MRs only", vec!["approve"]),
            ("send the summary to the team channel", vec!["send"]),
        ] {
            assert_eq!(outward_actions(brief), actions, "{brief}");
        }
    }

    #[test]
    fn a_brief_that_only_reads_needs_no_rule() {
        for brief in [
            "summarise the weekend's chat activity",
            "check the postgres backups and tell me if any failed",
            "відповідно до плану, перевір білд",
            "every morning send me a summary",
            "post it to me in my topic",
            "list MRs awaiting my approval",
        ] {
            assert!(
                outward_actions(brief).is_empty(),
                "{brief}: {:?}",
                outward_actions(brief)
            );
        }
    }

    #[test]
    fn a_rule_is_one_line() {
        assert_eq!(check_text("  never approve  ").unwrap(), "never approve");
        assert!(check_text("never\napprove").is_err());
        assert!(check_text("   ").is_err());
        assert!(check_text(&"x".repeat(MAX_TEXT + 1)).is_err());
    }
}
