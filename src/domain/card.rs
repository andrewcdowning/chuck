//! Card content rules: checklist items, card kind, tag names, hashtags and auto-tag rules. Pure functions of text.

/// Parses `- [ ] text`, `* [x] text` (leading whitespace allowed). Returns (done, text).
pub fn parse_item(line: &str) -> Option<(bool, &str)> {
    let rest = line.trim_start();
    let rest = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* "))?.trim_start();
    let rest = rest.strip_prefix('[')?;
    let mut chars = rest.chars();
    let mark = chars.next()?;
    if chars.next()? != ']' || !matches!(mark, ' ' | 'x' | 'X') {
        return None;
    }
    let text = chars.as_str();
    let text = text.strip_prefix(' ').unwrap_or(text);
    Some((mark != ' ', text))
}

/// Number of unchecked checklist items in the body.
pub fn open_items(body: &str) -> i64 {
    body.lines().filter(|l| matches!(parse_item(l), Some((false, _)))).count() as i64
}

pub fn card_kind(body: &str, remind_at: Option<i64>, has_media: bool) -> &'static str {
    if remind_at.is_some() {
        "reminder"
    } else if body.lines().any(|l| parse_item(l).is_some()) {
        "todo"
    } else if has_media {
        "media"
    } else {
        "note"
    }
}

/// Tag names: letters, digits, `_` and `-`, lowercase, at most 40 characters.
pub fn normalize_tag(s: &str) -> Option<String> {
    let t = s.trim().trim_start_matches('#').to_lowercase();
    let ok = !t.is_empty() && t.chars().count() <= 40 && t.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-');
    ok.then_some(t)
}

/// True if any keyword of the rule appears in the (already lowercased) text.
pub fn rule_matches(pattern: &str, text_lower: &str) -> bool {
    pattern
        .split(|c| c == '|' || c == ',')
        .map(|k| k.trim().to_lowercase())
        .filter(|k| !k.is_empty())
        .any(|k| text_lower.contains(&k))
}

/// Extract `#tags` from text: lowercase, alphanumeric plus `_` and `-`, deduplicated.
pub fn extract_tags(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || chars[i - 1].is_whitespace();
        if chars[i] == '#' && at_boundary {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '-') {
                j += 1;
            }
            if j > i + 1 {
                let t: String = chars[i + 1..j].iter().collect::<String>().to_lowercase();
                if !out.contains(&t) {
                    out.push(t);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Tags written in the text plus tags added by auto-tag rules (`(pattern, tag)` pairs, in rule order).
pub fn tags_for(rules: &[(String, String)], body: &str) -> Vec<String> {
    let mut tags = extract_tags(body);
    let lower = body.to_lowercase();
    for (pattern, tag) in rules {
        if rule_matches(pattern, &lower) && !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(extract_tags("buy milk #Home #home #to-do"), vec!["home", "to-do"]);
        assert!(extract_tags("issue#12 and # alone").is_empty());
    }

    #[test]
    fn checklist_items() {
        assert_eq!(parse_item("- [ ] milk"), Some((false, "milk")));
        assert_eq!(parse_item("  * [x] eggs"), Some((true, "eggs")));
        assert_eq!(parse_item("- [X]done"), Some((true, "done")));
        assert!(parse_item("- [y] no").is_none());
        assert!(parse_item("text [ ] x").is_none());
        assert_eq!(open_items("Groceries\n- [ ] milk\n- [x] eggs\n* [ ] bread\ntext [ ] x"), 2);
        assert_eq!(open_items("nothing"), 0);
    }

    #[test]
    fn rules_kinds_and_tag_names() {
        assert!(rule_matches("jira, PR-|sprint", "reviewing pr-123 today"));
        assert!(!rule_matches("jira", "nothing here"));
        assert!(!rule_matches(" , |", "anything"));
        assert_eq!(normalize_tag("#Work-1").as_deref(), Some("work-1"));
        assert!(normalize_tag("a b").is_none());
        assert!(normalize_tag("").is_none());
        assert_eq!(card_kind("- [ ] a", None, false), "todo");
        assert_eq!(card_kind("x", Some(5), true), "reminder");
        assert_eq!(card_kind("- [ ] a", None, true), "todo");
        assert_eq!(card_kind("x", None, true), "media");
        assert_eq!(card_kind("x", None, false), "note");
    }

    #[test]
    fn rule_tags_follow_written_tags_without_duplicates() {
        let rules = vec![("jira, PR-".to_string(), "work".to_string()), ("milk".to_string(), "home".to_string())];
        assert_eq!(tags_for(&rules, "Fix PR-7 #urgent"), vec!["urgent", "work"]);
        assert_eq!(tags_for(&rules, "milk and #home"), vec!["home"], "a tag already written is not added twice");
        assert!(tags_for(&rules, "nothing").is_empty());
        assert_eq!(tags_for(&[], "#a #b"), vec!["a", "b"]);
    }
}

