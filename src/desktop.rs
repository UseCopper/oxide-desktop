//! What a desktop entry says about an app: its name, its icon, and how to start it.
//!
//! The compositor reports a window's app as an id, and the panel needs three things
//! from that id to be useful: a name to put in a menu, an icon to put beside it, and
//! a command to start the app with. All three come from the freedesktop desktop entry
//! system, so this resolves an app id to the entry that describes it.
//!
//! An app id is normally a desktop entry's file name with the extension off — the
//! xdg-shell spec asks clients to send it that way — so `firefox` is
//! `firefox.desktop`. Compositors are not all that careful about it, so a file
//! extension is tolerated on the way in as well, and the reverse-DNS ids some apps
//! send (`org.gnome.Nautilus`) are looked for under that name first and by the
//! stripped form second.
//!
//! Nothing here reads a file outside the search path below, and an entry that asks
//! not to be shown is skipped, so a lookup that returns something is a real, visible
//! application.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// An app the panel can show and start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopApp {
    /// The app id the compositor reports, which is what this was looked up by.
    pub id: String,
    /// The entry's `Name`, for a menu row.
    pub name: String,
    /// The entry's `Icon`, which is usually a bare name for the icon theme rather
    /// than a path. See [`icon_name`].
    pub icon: String,
    /// The entry's `Exec`, with its field codes already removed, ready to run.
    pub exec: String,
}

impl DesktopApp {
    /// The name to show for this app, falling back to the id.
    ///
    /// An entry with no `Name` is not legal, but an entry we cannot read fully is
    /// still better than nothing on screen.
    pub fn label(&self) -> &str {
        if self.name.is_empty() { &self.id } else { &self.name }
    }

    /// The command to run, split into program and arguments.
    ///
    /// An `Exec` is a command line, not a program path, and may be quoted. It is
    /// handed to [`std::process::Command`] as a program plus arguments rather than
    /// through a shell, so a name with a space in it does not become two words.
    pub fn command(&self) -> Option<(String, Vec<String>)> {
        let mut parts = split_command(&self.exec);
        if parts.is_empty() {
            return None;
        }
        let program = parts.remove(0);
        Some((program, parts))
    }
}

/// Whether an app id names a real application at all.
///
/// Windows with no app id are grouped under a key of their own, prefixed with `#`, so
/// that they do not all collapse into one group. Such a key is not an app and has no
/// entry, cannot be launched and cannot be pinned.
pub fn is_app_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('#')
}

/// The directories desktop entries are looked for in, most specific first.
///
/// The first two are the user's own, which is where an entry that is not installed
/// system-wide lives, and which therefore has to win over a system entry of the same
/// name. `XDG_DATA_HOME` and `XDG_DATA_DIRS` are the configured places; the rest are
/// the defaults for when they are unset.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        dirs.push(data_home.join("applications"));
    } else if let Some(home) = &home {
        dirs.push(home.join(".local/share/applications"));
    }
    match std::env::var_os("XDG_DATA_DIRS") {
        Some(list) => {
            for dir in std::env::split_paths(&list) {
                dirs.push(dir.join("applications"));
            }
        }
        None => {
            dirs.push(PathBuf::from("/usr/local/share/applications"));
            dirs.push(PathBuf::from("/usr/share/applications"));
        }
    }
    if let Some(home) = &home {
        dirs.push(home.join(".local/share/applications"));
    }
    dirs
}

/// What to try when looking up an app id, in order.
///
/// An id that already ends in `.desktop` is the file name itself. Otherwise the spec
/// form is tried first, and then the id with its reverse-DNS prefix and every dot in
/// the remainder turned into dashes, which is what several desktops used before the
/// spec settled on the id being the file name.
pub fn candidate_names(id: &str) -> Vec<String> {
    let trimmed = id.strip_suffix(".desktop").unwrap_or(id);
    let mut names = Vec::new();
    // Every suffix of the id, shortest-prefix-stripped first: the whole id, then the
    // id without `org.`, then without `org.gnome.`, and so on. Each is tried both as
    // written and with its dots turned into dashes, which is what a desktop that
    // pre-dates the spec would have called the file.
    let mut suffix = trimmed;
    loop {
        for name in [
            suffix.to_string(),
            // Dashed and lowercased: that is the convention the file names using it
            // follow, so `org.gnome.Nautilus` is `gnome-nautilus.desktop`.
            suffix.replace('.', "-").to_lowercase(),
        ] {
            let name = format!("{name}.desktop");
            if !names.contains(&name) {
                names.push(name);
            }
        }
        match suffix.split_once('.') {
            Some((first, rest)) if !first.is_empty() && !rest.is_empty() => suffix = rest,
            _ => break,
        }
    }
    names
}

/// Find the entry for an app id.
///
/// Returns `None` for a key that is not an app id, for an id no installed entry
/// matches, and for an entry that asks not to be displayed — none of which are
/// failures, just apps the panel has nothing to show for.
pub fn lookup(id: &str) -> Option<DesktopApp> {
    if !is_app_id(id) {
        return None;
    }
    for name in candidate_names(id) {
        if let Some(app) = find_file(id, &search_dirs(), &name, 0) {
            return Some(app);
        }
    }
    None
}

/// Every app with a visible desktop entry, in the order the directories are searched.
///
/// This is what a "pin an app" list has to be built from: the panel can only offer to
/// pin an app it knows how to start.
pub fn installed() -> Vec<DesktopApp> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in search_dirs() {
        collect(&dir, &mut apps, &mut seen, 0);
    }
    apps
}

fn collect(dir: &Path, apps: &mut Vec<DesktopApp>, seen: &mut HashSet<String>, depth: u32) {
    // Entries are grouped in subdirectories in the wild (`kde4/`, `gnome/`), so this
    // goes a little way into them. Bounded, because a search that can walk a whole
    // filesystem is a search that can hang the panel.
    const MAX_DEPTH: u32 = 2;
    let Ok(children) = std::fs::read_dir(dir) else {
        return;
    };
    // Sorted, so the order of apps in a menu is the same on every run rather than
    // whatever order the filesystem happened to hand back.
    let mut paths: Vec<PathBuf> = children.filter_map(|child| child.ok().map(|c| c.path())).collect();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if depth < MAX_DEPTH {
                collect(&path, apps, seen, depth + 1);
            }
            continue;
        }
        if !name.ends_with(".desktop") {
            continue;
        }
        let id = name.trim_end_matches(".desktop");
        if !seen.insert(id.to_string()) {
            continue;
        }
        if let Some(app) = read_entry(id, &path) {
            apps.push(app);
        }
    }
}

/// Look for one file name across the search path, and the shallow subdirectories of
/// each, and read it if it is there.
fn find_file(id: &str, dirs: &[PathBuf], name: &str, depth: u32) -> Option<DesktopApp> {
    const MAX_DEPTH: u32 = 2;
    for dir in dirs {
        let path = dir.join(name);
        if path.is_file()
            && let Some(app) = read_entry(id, &path)
        {
            return Some(app);
        }
        if depth >= MAX_DEPTH {
            continue;
        }
        let Ok(children) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> =
            children.filter_map(|child| child.ok().map(|c| c.path())).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() && find_file(id, dirs, name, depth + 1).is_some() {
                return find_file(id, dirs, name, depth + 1);
            }
        }
    }
    None
}

/// Read one desktop entry, if it is an application that wants to be shown.
fn read_entry(id: &str, path: &Path) -> Option<DesktopApp> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_entry(id, &text)
}

/// Parse the text of a desktop entry.
///
/// Only the `[Desktop Entry]` group is read. A `Name` inside another group belongs to
/// something else — an action's own name, or a translated string for a link — and
/// taking one of those as the app's name is how a menu ends up labelled "Open in New
/// Window".
pub fn parse_entry(id: &str, text: &str) -> Option<DesktopApp> {
    let mut in_group = false;
    let mut name = String::new();
    let mut icon = String::new();
    let mut exec = String::new();
    let mut kind = String::new();

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }
        // A comment, or a line without a separator.
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        // A localised key, `Name[de]`, is not the plain name.
        if key.contains('[') {
            continue;
        }
        let value = value.trim();
        match key.trim() {
            "Name" => name = value.to_string(),
            "Icon" => icon = value.to_string(),
            "Exec" => exec = value.to_string(),
            "Type" => kind = value.to_string(),
            // An entry that is not an application has nothing to run. An entry that
            // asks not to be shown is not one to offer to pin.
            "NoDisplay" | "Hidden" if value.eq_ignore_ascii_case("true") => return None,
            _ => {}
        }
    }

    if !kind.is_empty() && kind != "Application" {
        return None;
    }
    if exec.is_empty() {
        return None;
    }
    let exec = strip_field_codes(&exec);
    if exec.trim().is_empty() {
        return None;
    }
    Some(DesktopApp {
        id: id.to_string(),
        name,
        icon,
        exec,
    })
}

/// Remove the field codes from an `Exec` line.
///
/// `%f`, `%u` and the rest stand for a file, a URL or an icon that the caller would
/// supply. Nothing supplies one here, so they go; `%%` is a literal percent and stays.
/// The codes are removed rather than replaced with a placeholder so that an entry
/// written for files is not left with a stray empty argument.
pub fn strip_field_codes(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // `%%`, and a trailing `%` that has nothing after it.
            Some('%') | None => out.push('%'),
            // A field code, or `%` followed by something that is not one: either way
            // the `%` and the character go, so an unknown code cannot reach a shell.
            Some(_) => {}
        }
    }
    out.trim().to_string()
}

/// The icon name to ask the icon theme for.
///
/// An entry's `Icon` is usually a bare name, but it may be an absolute path, and it
/// may carry an extension. A path is left alone for the caller to load directly; a
/// name has its extension dropped, because the icon theme is indexed by name and
/// `firefox.png` is not a name it knows. Both are returned, the bare name first,
/// since a themed name resolves where a path would not.
pub fn icon_name(icon: &str) -> (String, Option<PathBuf>) {
    let icon = icon.trim();
    if icon.is_empty() {
        return (String::new(), None);
    }
    if icon.starts_with('/') {
        return (String::new(), Some(PathBuf::from(icon)));
    }
    let bare = match icon.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty()
                && extension.len() <= 4
                && extension.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            stem.to_string()
        }
        _ => icon.to_string(),
    };
    (bare, None)
}

/// Split a command line into words, honouring quotes and backslash escapes.
///
/// `Exec` is a command line, and an entry is entitled to quote an argument that
/// contains a space. This is not a shell and never grows one: nothing here can start
/// a pipeline or a substitution, which is the point of not handing the string to
/// `/bin/sh`.
pub fn split_command(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            // The quote itself is not part of the word.
            '\'' | '"' => {
                started = true;
                while let Some(inner) = chars.next() {
                    if inner == c {
                        break;
                    }
                    // Only a double quote escapes; a backslash inside single quotes
                    // is a backslash, as in a shell.
                    if inner == '\\' && c == '"' {
                        match chars.next() {
                            Some(next) => word.push(next),
                            None => break,
                        }
                        continue;
                    }
                    word.push(inner);
                }
            }
            '\\' => {
                started = true;
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIREFOX: &str = "\
[Desktop Entry]
Name=Firefox
GenericName=Web Browser
Comment=Browse the World Wide Web
Exec=firefox %u
Icon=firefox
Terminal=false
Type=Application
Categories=Network;WebBrowser;
MimeType=text/html;x-scheme-handler/http;
StartupNotify=true
";

    const KONSOLE: &str = "\
[Desktop Entry]
Name=Konsole
Name[de]=Konsole
Exec=konsole
Icon=org.kde.konsole
Type=Application
X-KDE-StartupNotify=false
";

    const HIDDEN: &str = "\
[Desktop Entry]
Name=Hidden One
Exec=hidden
Type=Application
NoDisplay=true
";

    const NOT_AN_APP: &str = "\
[Desktop Entry]
Name=A Link
Type=Link
URL=https://example.com/
";

    const NO_EXEC: &str = "\
[Desktop Entry]
Name=Nothing To Run
Type=Application
";

    const WITH_ACTION: &str = "\
[Desktop Entry]
Name=Files
Exec=nautilus %U
Icon=org.gnome.Nautilus
Type=Application
Actions=new-window;

[Desktop Action new-window]
Name=New Window
Exec=nautilus --new-window
Icon=document-new
";

    fn parse(id: &str, text: &str) -> DesktopApp {
        parse_entry(id, text).expect("entry")
    }

    #[test]
    fn an_entry_gives_its_name_icon_and_command() {
        let app = parse("firefox", FIREFOX);
        assert_eq!(app.name, "Firefox");
        assert_eq!(app.icon, "firefox");
        assert_eq!(app.exec, "firefox");
        assert_eq!(app.label(), "Firefox");
    }

    #[test]
    fn a_translated_name_is_not_the_apps_name() {
        // `Name[de]` is a different key, and taking it would give the app's name in
        // whatever language the entry happened to carry.
        let app = parse("konsole", KONSOLE);
        assert_eq!(app.name, "Konsole");
    }

    #[test]
    fn a_name_from_another_group_is_not_the_apps_name() {
        // The action's name is "New Window", not "Files".
        let app = parse("org.gnome.Nautilus", WITH_ACTION);
        assert_eq!(app.name, "Files");
        assert_eq!(app.exec, "nautilus");
    }

    #[test]
    fn an_entry_that_asks_not_to_be_shown_is_not_an_app() {
        // Nothing to offer to pin, and nothing to launch from a menu.
        assert_eq!(parse_entry("hidden", HIDDEN), None);
    }

    #[test]
    fn an_entry_that_is_not_an_application_is_not_an_app() {
        assert_eq!(parse_entry("link", NOT_AN_APP), None);
    }

    #[test]
    fn an_entry_with_nothing_to_run_is_not_an_app() {
        assert_eq!(parse_entry("nothing", NO_EXEC), None);
    }

    #[test]
    fn a_window_with_no_app_id_is_not_an_app() {
        // The panel groups those under a key of its own, prefixed with a `#`.
        assert!(!is_app_id("#17"));
        assert!(!is_app_id(""));
        assert!(is_app_id("firefox"));
    }

    #[test]
    fn field_codes_go_and_a_literal_percent_stays() {
        assert_eq!(strip_field_codes("firefox %u"), "firefox");
        assert_eq!(strip_field_codes("gimp %U"), "gimp");
        assert_eq!(strip_field_codes("app %f %U %i %c %k"), "app");
        assert_eq!(strip_field_codes("app 100%%"), "app 100%");
        // A trailing `%`, and a code that does not exist, must not leave a stray
        // character behind to be run.
        assert_eq!(strip_field_codes("app 50%"), "app 50%");
        assert_eq!(strip_field_codes("app %z"), "app");
    }

    #[test]
    fn a_quoted_argument_is_one_word() {
        assert_eq!(
            split_command("app \"two words\" three"),
            ["app", "two words", "three"]
        );
        assert_eq!(split_command("app 'two words'"), ["app", "two words"]);
        // A backslash inside single quotes is a backslash.
        assert_eq!(split_command("app 'a\\b'"), ["app", "a\\b"]);
        assert_eq!(split_command("app a\\ b"), ["app", "a b"]);
    }

    #[test]
    fn a_command_splits_into_a_program_and_its_arguments() {
        let app = parse("files", WITH_ACTION);
        let (program, arguments) = app.command().expect("a command");
        assert_eq!(program, "nautilus");
        assert!(arguments.is_empty());
    }

    #[test]
    fn an_icon_name_loses_its_extension_and_a_path_is_kept() {
        assert_eq!(icon_name("firefox"), ("firefox".into(), None));
        assert_eq!(icon_name("firefox.png"), ("firefox".into(), None));
        assert_eq!(icon_name(""), (String::new(), None));
        // A path is not a theme name; the caller has to load it.
        let (name, path) = icon_name("/usr/share/icons/hicolor/48x48/app.png");
        assert!(name.is_empty());
        assert_eq!(path, Some(PathBuf::from("/usr/share/icons/hicolor/48x48/app.png")));
        // A dot in a name that is not an extension is part of the name.
        assert_eq!(icon_name("org.gnome.Nautilus"), ("org.gnome.Nautilus".into(), None));
    }

    #[test]
    fn an_id_is_looked_for_under_the_names_it_might_have() {
        // The spec form first, then the file name the id's prefix would produce.
        let names = candidate_names("org.gnome.Nautilus");
        assert_eq!(names[0], "org.gnome.Nautilus.desktop");
        assert!(names.contains(&"gnome-nautilus.desktop".to_string()));
        // An id that already carries the extension is the file name itself.
        assert_eq!(candidate_names("firefox.desktop"), ["firefox.desktop"]);
        assert_eq!(candidate_names("firefox"), ["firefox.desktop"]);
    }

    #[test]
    fn an_app_with_no_name_falls_back_to_its_id() {
        let app = DesktopApp {
            id: "mystery".into(),
            name: String::new(),
            icon: String::new(),
            exec: "mystery".into(),
        };
        assert_eq!(app.label(), "mystery");
    }
}
