//! Markdown export. Pure functions so any `NoteStore` can be exported the same way.

use crate::store::{Note, TodoEntry};

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `2026-11-26 15:41 UTC`
pub fn fmt_utc(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", t / 3600, (t % 3600) / 60)
}

pub fn fmt_date(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Renders notes oldest first. Each note becomes a section: a heading with its creation time, a metadata
/// line (tags, reminder), then the body verbatim. Checklists are already Markdown task lists.
pub fn to_markdown(notes: &[Note], todos: &[TodoEntry], now_secs: i64) -> String {
    let mut out = format!("# Chuck export\n\nExported {} · {} notes\n", fmt_utc(now_secs), notes.len());
    if !todos.is_empty() {
        out.push_str("\n---\n\n## Todos\n\n");
        for t in todos {
            out.push_str(&format!("- [{}] {}\n", if t.done { "x" } else { " " }, t.text));
        }
    }
    let mut ordered: Vec<&Note> = notes.iter().collect();
    ordered.sort_by_key(|n| (n.created_at, n.id));
    for n in ordered {
        out.push_str(&format!("\n---\n\n## {}\n\n", fmt_utc(n.created_at)));
        let mut meta = Vec::new();
        if !n.tags.is_empty() {
            meta.push(format!("Tags: {}", n.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ")));
        }
        if let Some(at) = n.remind_at {
            let mut r = format!("Reminder: {}", fmt_utc(at));
            if let Some(rep) = &n.repeat {
                r.push_str(&format!(" (repeats {rep})"));
            }
            if n.reminder_done {
                r.push_str(" [done]");
            }
            meta.push(r);
        }
        if !meta.is_empty() {
            out.push_str(&meta.iter().map(|m| format!("*{m}*")).collect::<Vec<_>>().join("  \n"));
            out.push_str("\n\n");
        }
        out.push_str(n.body.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: i64, body: &str, created: i64) -> Note {
        Note {
            id,
            kind: "note",
            body: body.into(),
            tags: vec![],
            created_at: created,
            updated_at: created,
            remind_at: None,
            repeat: None,
            reminder_done: false,
        }
    }

    #[test]
    fn dates() {
        assert_eq!(fmt_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(fmt_utc(1_795_732_903), "2026-11-26 22:41 UTC");
        assert_eq!(fmt_date(951_782_400), "2000-02-29"); // leap day
        assert_eq!(fmt_utc(-1), "1969-12-31 23:59 UTC");
    }

    #[test]
    fn markdown_layout() {
        let mut a = note(2, "Groceries #home\n- [ ] milk\n- [x] eggs\n", 2_000_000_000);
        a.tags = vec!["home".into()];
        let mut b = note(1, "Standup", 1_000_000_000);
        b.remind_at = Some(4_102_444_800);
        b.repeat = Some("weekdays".into());
        let todos = vec![TodoEntry { id: 1, text: "call Bob".into(), done: false, created_at: 0 }, TodoEntry { id: 2, text: "buy milk".into(), done: true, created_at: 0 }];
        let md = to_markdown(&[a, b], &todos, 1_795_732_903);
        assert!(md.contains("## Todos\n\n- [ ] call Bob\n- [x] buy milk\n"));
        assert!(md.starts_with("# Chuck export\n\nExported 2026-11-26 22:41 UTC · 2 notes\n"));
        // oldest first
        assert!(md.find("Standup").unwrap() < md.find("Groceries").unwrap());
        assert!(md.contains("*Tags: #home*"));
        assert!(md.contains("*Reminder: 2100-01-01 00:00 UTC (repeats weekdays)*"));
        assert!(md.contains("- [ ] milk\n- [x] eggs\n"));
        assert!(!md.contains("eggs\n\n\n"));
    }

    #[test]
    fn empty_export() {
        let md = to_markdown(&[], &[], 0);
        assert!(md.contains("0 notes") && !md.contains("Todos"));
    }
}
