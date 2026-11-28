//! The page and the files it loads. Served without signing in, like the page itself: they hold no data, and the API calls the
//! scripts make are authenticated one by one.
//!
//! The page is `index.html` plus one stylesheet and several scripts under `/assets/`. The scripts are plain (not modules) and
//! are loaded in the order `index.html` lists them, so they share one global scope, as the single script they were cut from
//! did. Each file starts with a line saying what it holds. To add a file, put it in `static/`, list it here and in
//! `index.html`; the tests below fail if the two disagree.

pub struct Asset {
    pub name: &'static str,
    pub content_type: &'static str,
    pub body: &'static str,
}

const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

pub const INDEX: &str = include_str!("../static/index.html");

pub const ASSETS: &[Asset] = &[
    Asset { name: "app.css", content_type: CSS, body: include_str!("../static/app.css") },
    Asset { name: "helpers.js", content_type: JS, body: include_str!("../static/helpers.js") },
    Asset { name: "media.js", content_type: JS, body: include_str!("../static/media.js") },
    Asset { name: "audio.js", content_type: JS, body: include_str!("../static/audio.js") },
    Asset { name: "composer.js", content_type: JS, body: include_str!("../static/composer.js") },
    Asset { name: "cards.js", content_type: JS, body: include_str!("../static/cards.js") },
    Asset { name: "suggestions.js", content_type: JS, body: include_str!("../static/suggestions.js") },
    Asset { name: "settings.js", content_type: JS, body: include_str!("../static/settings.js") },
    Asset { name: "moving.js", content_type: JS, body: include_str!("../static/moving.js") },
    Asset { name: "feed.js", content_type: JS, body: include_str!("../static/feed.js") },
    Asset { name: "reminders.js", content_type: JS, body: include_str!("../static/reminders.js") },
    Asset { name: "app.js", content_type: JS, body: include_str!("../static/app.js") },
];

pub fn find(name: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|a| a.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `/assets/...` files the page asks for, in the order it asks.
    fn referenced() -> Vec<String> {
        INDEX.split("/assets/").skip(1).map(|r| r.split('"').next().unwrap().to_string()).collect()
    }

    #[test]
    fn the_page_asks_for_exactly_the_files_there_are() {
        let asked = referenced();
        let mut have: Vec<&str> = ASSETS.iter().map(|a| a.name).collect();
        let mut want: Vec<&str> = asked.iter().map(String::as_str).collect();
        have.sort();
        want.sort();
        assert_eq!(want, have, "index.html and assets.rs disagree");
        assert!(asked.windows(2).all(|w| w[0] != w[1]), "a file is listed twice");
        assert_eq!(asked[0], "app.css");
        assert_eq!(asked.last().map(String::as_str), Some("app.js"), "start-up code runs last");
    }

    #[test]
    fn the_page_keeps_only_the_theme_script_inline() {
        assert_eq!(INDEX.matches("<script>").count(), 1, "everything else belongs in a file");
        assert!(INDEX.find("<script>").unwrap() < INDEX.find("app.css").unwrap(), "the theme is set before the stylesheet paints");
        assert!(!INDEX.contains("<style>"));
    }

    #[test]
    fn files_are_not_empty_and_say_what_they_hold() {
        for a in ASSETS {
            assert!(a.body.len() > 100, "{} is nearly empty", a.name);
            if a.name.ends_with(".js") {
                assert!(a.body.starts_with("// "), "{} should start with a line saying what it holds", a.name);
            }
        }
    }

    #[test]
    fn nothing_but_known_names_is_served() {
        assert!(find("app.css").is_some());
        for bad in ["", "..", "../Cargo.toml", "app.js/", "index.html", "APP.JS", "app.js?x"] {
            assert!(find(bad).is_none(), "{bad}");
        }
    }
}
