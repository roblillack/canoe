//! What the XDG desktop entries say about an application, found by the app
//! id of its windows: the name it goes by and its icon.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// The parts of an application's desktop entry canoe uses.
#[derive(Debug, Default, PartialEq)]
pub struct AppInfo {
    /// `Name=`, in the user's language when the entry has it.
    pub name: Option<String>,
    /// `Icon=`: a name to look up in the icon theme, or an absolute path.
    pub icon: Option<String>,
}

thread_local! {
    /// Desktop entries by app id, read once per app id. Apps installed after
    /// an app id was first looked up are not seen.
    static CACHE: RefCell<HashMap<String, Option<Rc<AppInfo>>>> = RefCell::new(HashMap::new());
}

/// The desktop entry of the application with this app id.
pub fn app_info(app_id: &str) -> Option<Rc<AppInfo>> {
    if app_id.is_empty() {
        return None;
    }
    if let Some(cached) = CACHE.with(|cache| cache.borrow().get(app_id).cloned()) {
        return cached;
    }
    let info = find_desktop_entry(&xdg_data_dirs(), app_id)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|content| Rc::new(parse_desktop_entry(&content, &locale_variants())));
    CACHE.with(|cache| {
        cache.borrow_mut().insert(app_id.to_string(), info.clone());
    });
    info
}

/// The name of the application with this app id.
pub fn app_name(app_id: &str) -> Option<String> {
    app_info(app_id)?.name.clone()
}

/// Find the desktop entry for `app_id`: `<app-id>.desktop`, the same in
/// lower case, or else the entry whose `StartupWMClass=` is the app id.
/// Earlier data directories win.
fn find_desktop_entry(dirs: &[PathBuf], app_id: &str) -> Option<PathBuf> {
    let mut names = vec![format!("{app_id}.desktop")];
    let lower = format!("{}.desktop", app_id.to_lowercase());
    if lower != names[0] {
        names.push(lower);
    }
    for name in &names {
        for dir in dirs {
            let path = dir.join("applications").join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir.join("applications")) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "desktop"))
            .collect();
        paths.sort();
        for path in paths {
            if startup_wm_class(&path).is_some_and(|class| class.eq_ignore_ascii_case(app_id)) {
                return Some(path);
            }
        }
    }
    None
}

fn startup_wm_class(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let class = desktop_entry_lines(&content)
        .find_map(|(key, value)| (key == "StartupWMClass").then(|| value.to_string()));
    class
}

/// The key/value lines of a desktop file's `[Desktop Entry]` group.
fn desktop_entry_lines(content: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut in_entry = false;
    content.lines().filter_map(move |line| {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            return None;
        }
        if !in_entry || line.starts_with('#') {
            return None;
        }
        let (key, value) = line.split_once('=')?;
        Some((key.trim(), value.trim()))
    })
}

/// Read a desktop file. `locales` are the `Name[...]` suffixes to prefer,
/// best first.
fn parse_desktop_entry(content: &str, locales: &[String]) -> AppInfo {
    let mut name = None;
    // The best localized name so far, by its place in `locales`.
    let mut localized: Option<(usize, String)> = None;
    let mut icon = None;
    for (key, value) in desktop_entry_lines(content) {
        if value.is_empty() {
            continue;
        }
        if key == "Name" {
            name.get_or_insert_with(|| value.to_string());
        } else if key == "Icon" {
            icon.get_or_insert_with(|| value.to_string());
        } else if let Some(locale) = key.strip_prefix("Name[").and_then(|k| k.strip_suffix(']')) {
            if let Some(rank) = locales.iter().position(|l| l == locale) {
                if localized.as_ref().is_none_or(|(best, _)| rank < *best) {
                    localized = Some((rank, value.to_string()));
                }
            }
        }
    }
    AppInfo {
        name: localized.map(|(_, name)| name).or(name),
        icon,
    }
}

/// The `[locale]` suffixes of localized keys that match the user's
/// language, best first, as the desktop entry spec matches them:
/// `lang_COUNTRY@MODIFIER`, `lang_COUNTRY`, `lang@MODIFIER`, `lang`.
fn locale_variants() -> Vec<String> {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    locale_variants_of(&locale)
}

fn locale_variants_of(locale: &str) -> Vec<String> {
    // Drop the encoding: `de_DE.UTF-8@euro` → `de_DE@euro`.
    let (base, modifier) = match locale.split_once('@') {
        Some((base, modifier)) => (base, Some(modifier)),
        None => (locale, None),
    };
    let base = base.split('.').next().unwrap_or_default();
    if base.is_empty() || base == "C" || base == "POSIX" {
        return Vec::new();
    }
    let (lang, country) = match base.split_once('_') {
        Some((lang, country)) => (lang, Some(country)),
        None => (base, None),
    };
    let mut variants = Vec::new();
    if let (Some(country), Some(modifier)) = (country, modifier) {
        variants.push(format!("{lang}_{country}@{modifier}"));
    }
    if let Some(country) = country {
        variants.push(format!("{lang}_{country}"));
    }
    if let Some(modifier) = modifier {
        variants.push(format!("{lang}@{modifier}"));
    }
    variants.push(lang.to_string());
    variants
}

/// Return XDG data directories: $XDG_DATA_HOME then $XDG_DATA_DIRS.
pub(super) fn xdg_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // $XDG_DATA_HOME (default: $HOME/.local/share)
    if let Ok(v) = std::env::var("XDG_DATA_HOME") {
        if !v.is_empty() {
            dirs.push(PathBuf::from(v));
        }
    } else if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local").join("share"));
    }

    // $XDG_DATA_DIRS (default: /usr/local/share:/usr/share)
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    if data_dirs.is_empty() {
        dirs.push(PathBuf::from("/usr/local/share"));
        dirs.push(PathBuf::from("/usr/share"));
    } else {
        for p in data_dirs.split(':') {
            if !p.is_empty() {
                dirs.push(PathBuf::from(p));
            }
        }
    }

    dirs
}

/// Separators between the parts of window titles, like the one in
/// "Page — Mozilla Firefox".
const TITLE_SEPARATORS: &[&str] = &[" — ", " – ", " -- ", " - ", " · ", " | ", ": "];

/// The separator after the application's name in titles canoe writes.
const APP_NAME_SEPARATOR: &str = " — ";

/// A window's title as window lists show it: led by the name of its
/// application. A title that ends with the application's name, as
/// browsers' titles do ("Page — Mozilla Firefox"), has it moved to the
/// front ("Mozilla Firefox — Page"); one that names the application nowhere
/// gets `app_name` put in front ("~: fish" → "Alacritty — ~: fish"). Titles
/// may also name the application by its `app_id` ("Photo – GIMP" for
/// `org.gimp.GIMP`, named "GNU Image Manipulation Program").
pub fn title_with_app_name(title: &str, app_name: &str, app_id: &str) -> String {
    let title = title.trim();
    let app_name = app_name.trim();
    if app_name.is_empty() {
        return title.to_string();
    }
    if title.is_empty() {
        return app_name.to_string();
    }
    // Reverse-DNS ids name the application last.
    let app_id = app_id.rsplit('.').next().unwrap_or_default();
    let names_app = |text: &str| names_app(text, app_name) || names_app(text, app_id);

    // Already led by the application's name: only the separator changes.
    if let Some((at, sep)) = find_separator(title, false) {
        let (head, rest) = (&title[..at], &title[at + sep.len()..]);
        if names_app(head) && !rest.trim().is_empty() {
            return format!("{head}{APP_NAME_SEPARATOR}{rest}");
        }
    }
    // Ending with it: moved to the front.
    if let Some((at, sep)) = find_separator(title, true) {
        let (rest, tail) = (&title[..at], &title[at + sep.len()..]);
        if names_app(tail) && !rest.trim().is_empty() {
            return format!("{tail}{APP_NAME_SEPARATOR}{rest}");
        }
    }
    if names_app(title) {
        return title.to_string();
    }
    format!("{app_name}{APP_NAME_SEPARATOR}{title}")
}

/// The first (or with `last`, the last) separator in `title`: its byte
/// offset and which one it is.
fn find_separator(title: &str, last: bool) -> Option<(usize, &'static str)> {
    let found = TITLE_SEPARATORS.iter().filter_map(|sep| {
        let at = if last {
            title.rfind(sep)
        } else {
            title.find(sep)
        }?;
        Some((at, *sep))
    });
    // No two separators match at the same offset.
    if last {
        found.max_by_key(|(at, _)| *at)
    } else {
        found.min_by_key(|(at, _)| *at)
    }
}

/// Whether `text` names the application `app_name`: it shares a word with
/// it, ignoring case, as "Mozilla Firefox" does with "Firefox ESR".
/// Words of fewer than three letters don't count.
fn names_app(text: &str, app_name: &str) -> bool {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() >= 3)
            .map(str::to_lowercase)
            .collect()
    };
    let app_words = words(app_name);
    words(text).iter().any(|w| app_words.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_titles_without_the_app_name() {
        assert_eq!(
            title_with_app_name("~: fish", "Alacritty", ""),
            "Alacritty — ~: fish"
        );
        assert_eq!(
            title_with_app_name("notes.txt", "Text Editor", ""),
            "Text Editor — notes.txt"
        );
    }

    #[test]
    fn moves_a_trailing_app_name_to_the_front() {
        assert_eq!(
            title_with_app_name("Page — Mozilla Firefox", "Firefox ESR", ""),
            "Mozilla Firefox — Page"
        );
        assert_eq!(
            title_with_app_name("Inbox - Gmail - Chromium", "Chromium Web Browser", ""),
            "Chromium — Inbox - Gmail"
        );
        assert_eq!(
            title_with_app_name(
                "main.rs - canoe - Visual Studio Code",
                "Visual Studio Code",
                ""
            ),
            "Visual Studio Code — main.rs - canoe"
        );
        assert_eq!(
            title_with_app_name(
                "Photo · GIMP",
                "GNU Image Manipulation Program",
                "org.gimp.GIMP"
            ),
            "GIMP — Photo"
        );
    }

    #[test]
    fn keeps_a_leading_app_name() {
        assert_eq!(
            title_with_app_name("Alacritty - ~/dev", "Alacritty", ""),
            "Alacritty — ~/dev"
        );
        assert_eq!(
            title_with_app_name("Alacritty: ~", "Alacritty", ""),
            "Alacritty — ~"
        );
    }

    #[test]
    fn leaves_titles_that_name_the_app_elsewhere() {
        assert_eq!(
            title_with_app_name("Alacritty", "Alacritty", ""),
            "Alacritty"
        );
        assert_eq!(
            title_with_app_name("Mozilla Firefox", "Firefox ESR", ""),
            "Mozilla Firefox"
        );
        assert_eq!(
            title_with_app_name("Welcome to Firefox", "Firefox ESR", ""),
            "Welcome to Firefox"
        );
    }

    #[test]
    fn hyphens_inside_words_are_not_separators() {
        assert_eq!(
            title_with_app_name("read-only-file", "Alacritty", ""),
            "Alacritty — read-only-file"
        );
    }

    #[test]
    fn empty_titles_and_names() {
        assert_eq!(title_with_app_name("", "Alacritty", ""), "Alacritty");
        assert_eq!(title_with_app_name("~: fish", "", ""), "~: fish");
    }

    #[test]
    fn parses_names_in_the_users_language() {
        let content = "\
[Desktop Entry]
Name=Files
Name[de]=Dateien
Name[de_AT]=Dateien (AT)
Icon=org.gnome.Nautilus

[Desktop Action new-window]
Name=New Window
";
        let parse = |locale: &str| parse_desktop_entry(content, &locale_variants_of(locale));
        assert_eq!(parse("de_DE.UTF-8").name.as_deref(), Some("Dateien"));
        assert_eq!(parse("de_AT.UTF-8").name.as_deref(), Some("Dateien (AT)"));
        assert_eq!(parse("en_US.UTF-8").name.as_deref(), Some("Files"));
        assert_eq!(parse("C").name.as_deref(), Some("Files"));
        assert_eq!(parse("").icon.as_deref(), Some("org.gnome.Nautilus"));
    }

    #[test]
    fn locale_variants_follow_the_spec() {
        assert_eq!(
            locale_variants_of("sr_RS.UTF-8@latin"),
            ["sr_RS@latin", "sr_RS", "sr@latin", "sr"]
        );
        assert_eq!(locale_variants_of("de"), ["de"]);
        assert!(locale_variants_of("POSIX").is_empty());
    }

    #[test]
    fn finds_entries_by_lower_case_id_and_wm_class() {
        let root = std::env::temp_dir().join(format!("canoe-app-info-{}", std::process::id()));
        let apps = root.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(apps.join("slack.desktop"), "[Desktop Entry]\nName=Slack\n").unwrap();
        std::fs::write(
            apps.join("org.example.Term.desktop"),
            "[Desktop Entry]\nName=Term\nStartupWMClass=term-x\n",
        )
        .unwrap();
        let dirs = [root.clone()];

        assert_eq!(
            find_desktop_entry(&dirs, "Slack"),
            Some(apps.join("slack.desktop"))
        );
        assert_eq!(
            find_desktop_entry(&dirs, "term-x"),
            Some(apps.join("org.example.Term.desktop"))
        );
        assert_eq!(find_desktop_entry(&dirs, "nothing"), None);

        std::fs::remove_dir_all(&root).unwrap();
    }
}
