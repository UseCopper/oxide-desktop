//! What the panel remembers between runs: which apps are pinned, and the bar's order.
//!
//! The compositor sends the panel a snapshot of the open windows on every change, and
//! nothing in that says which apps are pinned or where their squares belong — both are
//! the panel's own business. Without somewhere to put them, pinning lasts until the
//! next restart and the order is whatever the first snapshot happened to say.
//!
//! A line-based file, because there is nothing here a parser library would help with:
//! one directive per line, and the only fiddly part is an app id, which is whatever
//! string a client passed to `set_app_id` and so cannot be trusted to be tidy.
//!
//! Nothing here is allowed to be fatal. A missing file is a panel with no pins. A
//! corrupt one is a panel with no pins. Neither may stop the bar from being built.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// What was remembered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PanelLayout {
    /// The bar's order, as app ids.
    pub order: Vec<String>,
    /// The apps whose square stays in the bar whether or not they have windows.
    pub pinned: HashSet<String>,
}

/// A path to use instead of the real one, for tests.
///
/// This exists because of a real, long-standing bug: the panel's tests exercise code that
/// saves the layout, and that save went to the *user's* actual `panel.conf`. Every
/// `cargo test` run therefore overwrote the running panel's pins and bar order with test
/// fixtures — silently, because the tests passed and the panel merely came back with the
/// wrong bar. Verified against the pre-existing code, not just the current one.
///
/// It is a thread-local rather than an environment variable so it cannot be set by
/// accident at runtime, and it is `#[cfg(test)]` so it is not in a release build at all.
#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Point [`path`] at a temporary file for the duration of a test.
///
/// Returns a guard that puts the real path back, so a test that panics still leaves the
/// next one looking at the real thing.
#[cfg(test)]
pub fn use_test_path(path: PathBuf) -> TestPathGuard {
    OVERRIDE.with(|slot| *slot.borrow_mut() = Some(path));
    TestPathGuard(())
}

/// Puts the real config path back when dropped.
#[cfg(test)]
pub struct TestPathGuard(());

#[cfg(test)]
impl Drop for TestPathGuard {
    fn drop(&mut self) {
        OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Where the file lives.
///
/// `XDG_CONFIG_HOME` is the configured place; `$HOME/.config` is the default for when it
/// is unset. A panel with no home directory has nowhere to keep anything, which is not a
/// reason to refuse to start.
pub fn path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = OVERRIDE.with(|slot| slot.borrow().clone()) {
        return Some(path);
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("oxide-desktop").join("panel.conf"))
}

/// Read what was remembered. A file that is not there, or not readable, is nothing
/// remembered.
pub fn load() -> PanelLayout {
    let Some(path) = path() else {
        return PanelLayout::default();
    };
    load_from(&path)
}

/// Read a layout from a given file, which is [`load`] with the path spelled out.
pub fn load_from(path: &Path) -> PanelLayout {
    let Ok(text) = std::fs::read_to_string(path) else {
        return PanelLayout::default();
    };
    parse(&text)
}

/// Write what is remembered now.
///
/// Best effort: a panel that cannot write its own state still works, and a failure
/// here is a preference lost, not a broken bar. Written through a temporary file and
/// renamed, so a panel killed mid-write leaves the previous file rather than half of
/// one.
pub fn save(layout: &PanelLayout) {
    let Some(path) = path() else {
        return;
    };
    save_to(path, layout);
}

/// [`save`] with the path spelled out, and reporting whether it worked.
pub fn save_to(path: PathBuf, layout: &PanelLayout) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let text = render(layout);
    let temporary = path.with_extension("conf.new");
    if std::fs::write(&temporary, text).is_err() {
        return false;
    }
    std::fs::rename(&temporary, &path).is_ok()
}

fn render(layout: &PanelLayout) -> String {
    let mut out = String::from("# Written by the panel. Safe to edit.\n");
    for id in &layout.order {
        out.push_str("order ");
        out.push_str(&encode(id));
        out.push('\n');
    }
    // Sorted, so a diff of this file is a diff of what changed and not of what order
    // a hash set happened to iterate in.
    let mut pinned: Vec<&String> = layout.pinned.iter().collect();
    pinned.sort();
    for id in pinned {
        out.push_str("pin ");
        out.push_str(&encode(id));
        out.push('\n');
    }
    out
}

/// Read a layout out of the file's text.
pub fn parse(text: &str) -> PanelLayout {
    let mut layout = PanelLayout::default();
    for line in text.lines() {
        let line = line.trim();
        // A comment, or a blank line.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((directive, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let Some(id) = decode(value) else {
            continue;
        };
        match directive {
            "order" => {
                // Named twice: the first mention is the place, the rest are ignored
                // rather than moving the app to the end of the bar.
                if !layout.order.iter().any(|entry| *entry == id) {
                    layout.order.push(id);
                }
            }
            "pin" => {
                layout.pinned.insert(id);
            }
            _ => {}
        }
    }
    layout
}

/// Encode an app id so it can be one field on one line.
///
/// An app id is whatever a client passed to `set_app_id`, so it may hold a space, a
/// `#` that would start a comment, or a newline that would end the line and start a
/// directive. Anything outside the characters that cannot be mistaken for structure
/// is percent-encoded, which is reversible and has no escaping rules to get wrong.
pub fn encode(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Decode an app id written by [`encode`]. `None` if it is not something [`encode`]
/// could have written.
pub fn decode(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = encoded.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A set of app ids, from the ones written in a test.
    fn sets(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn a_layout_survives_a_round_trip_through_the_file() {
        let layout = PanelLayout {
            order: vec![
                "firefox".to_string(),
                "org.gnome.Nautilus".to_string(),
                "an app with spaces".to_string(),
                "with#hash".to_string(),
                "with\ttab".to_string(),
                "ünïcode".to_string(),
            ],
            pinned: ["firefox".to_string(), "an app with spaces".to_string()]
                .into_iter()
                .collect(),
        };
        let text = render(&layout);
        // One directive per line, and nothing that could be read as a second one.
        assert_eq!(text.lines().filter(|line| line.is_empty()).count(), 0);
        assert_eq!(parse(&text), layout);
    }

    #[test]
    fn an_id_that_looks_like_structure_is_still_just_an_id() {
        // A newline in an id must not be able to write a directive.
        let id = "evil\norder other";
        let text = format!("order {}\n", encode(id));
        assert_eq!(text.lines().count(), 1);
        assert_eq!(parse(&text).order, [id]);
        // And a `#` must not be able to comment out the rest of its own line.
        let text = format!("order {}\n", encode("app # not a comment"));
        assert_eq!(parse(&text).order, ["app # not a comment"]);
    }

    #[test]
    fn an_app_named_twice_keeps_the_first_place_it_was_given() {
        // Otherwise a hand-edited file with the app listed twice would silently move
        // it to the end of the bar.
        let layout = parse("order a\norder b\norder a\n");
        assert_eq!(layout.order, ["a", "b"]);
    }

    #[test]
    fn pins_and_order_are_independent() {
        let layout = parse("order a\npin b\norder c\npin d\n");
        assert_eq!(layout.order, ["a", "c"]);
        assert_eq!(layout.pinned, sets(&["b", "d"]));
        // A pin with no place, and a place with no pin, are both allowed: an app can
        // keep its square while running without being pinned, and a pinned app whose
        // entry has been uninstalled keeps its place.
        assert!(!layout.order.contains(&"b".to_string()));
    }

    #[test]
    fn nothing_recognisable_is_nothing_remembered() {
        // Not an error, and not a reason to refuse the rest of the file.
        assert_eq!(parse(""), PanelLayout::default());
        assert_eq!(parse("\n\n   \n"), PanelLayout::default());
        assert_eq!(parse("# only a comment\n"), PanelLayout::default());
        // Directives with nothing after them, and ones that are not directives.
        assert_eq!(parse("order\norder   \nnonsense value\n"), PanelLayout::default());
        // A directive that is only half there.
        let layout = parse("order a\norder b\norder %ZZ\npin c\n");
        assert_eq!(layout.order, ["a", "b"]);
        assert_eq!(layout.pinned, sets(&["c"]));
    }

    #[test]
    fn a_file_that_is_not_there_is_a_panel_with_no_pins() {
        let missing = std::env::temp_dir().join("oxide-panel-definitely-not-here.conf");
        assert_eq!(load_from(&missing), PanelLayout::default());
    }

    #[test]
    fn saving_writes_something_that_reads_back_the_same() {
        let path = std::env::temp_dir().join(format!("oxide-panel-{}.conf", std::process::id()));
        let layout = PanelLayout {
            order: vec!["a b".to_string(), "c".to_string()],
            pinned: ["c".to_string()].into_iter().collect(),
        };
        assert!(save_to(path.clone(), &layout));
        assert_eq!(load_from(&path), layout);
        // And the temporary file it went through is not left behind.
        assert!(!path.with_extension("conf.new").exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn encoding_covers_exactly_what_decoding_accepts() {
        for id in ["", "a", "A.b_c-1", " ", "\n", "%", "%41", "ünïcode", "\u{1f600}"] {
            let encoded = encode(id);
            assert!(
                !encoded.contains([' ', '\n', '\t', '#']),
                "{encoded:?} could be read as structure"
            );
            assert_eq!(decode(&encoded).as_deref(), Some(id), "round trip of {id:?}");
        }
        // What cannot have come from `encode` is refused rather than guessed at.
        assert_eq!(decode("%ZZ"), None);
        assert_eq!(decode("%4"), None);
        // Non-UTF-8 is not something `encode` produces, so it is not accepted.
        assert_eq!(decode("%FF"), None);
    }
}
