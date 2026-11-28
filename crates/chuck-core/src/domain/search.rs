//! Free-text search input. Backends turn the terms into their own query syntax.
//!
//! Contract (checked for every backend by `conformance::search_contract`): each term matches the start of a word in the
//! card, case-insensitively; a card must match every term, in any order; a blank query means no filter.

/// The search terms in a query: whitespace-separated words with double quotes removed, empty ones dropped.
pub fn terms(q: &str) -> Vec<String> {
    q.split_whitespace().map(|w| w.replace('"', "")).filter(|w| !w.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_strips_quotes() {
        assert_eq!(terms("  dead  line "), vec!["dead", "line"]);
        assert_eq!(terms("\"quoted\" a\"b"), vec!["quoted", "ab"]);
        assert!(terms("   ").is_empty());
        assert!(terms("\" \"\"").is_empty());
        assert_eq!(terms("* OR ("), vec!["*", "OR", "("], "operators are ordinary words here; a backend must quote them");
    }
}
