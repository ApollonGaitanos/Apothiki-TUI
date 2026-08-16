//! User configuration (spec §9, §12/M5).
//!
//! Read from `$XDG_CONFIG_HOME/apothiki/config.toml`. Everything has a working
//! default, so the file is optional and a missing or broken one is never fatal —
//! a package explorer that refuses to start because of a typo in a colour name
//! would be a poor trade.
//!
//! What lives here is the set of decisions that are genuinely the user's:
//! filter lists that depend on taste, extra packages to protect, keys, and
//! colours. What does **not** live here is the hard denylist's core: no config
//! value can make `glibc` removable, because that would be a `--force` flag
//! wearing a different hat (spec §6.1).

use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub apps: AppsConfig,
    pub safety: SafetyConfig,
    pub aur: AurConfig,
    pub theme: ThemeConfig,
    /// Action name → key, e.g. `quit = "q"`.
    pub keys: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppsConfig {
    /// Package name suffixes folded into the application they belong to.
    pub merge_suffixes: Vec<String>,
    /// Desktop file ids to hide; `*` is a wildcard.
    pub noise: Vec<String>,
    /// Extra directories to search for AppImages.
    pub appimage_dirs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// Additional package names that may never be removed.
    ///
    /// Additive only. There is deliberately no way to *remove* something from
    /// the built-in denylist: an escape hatch in a config file is still an
    /// escape hatch, and the whole point of §6.1 is that it does not exist.
    pub also_protect: Vec<String>,
    /// Free more than this many bytes and the removal is treated as dangerous.
    pub dangerous_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AurConfig {
    /// Helper to drive. Empty means "detect".
    pub helper: String,
    /// How many hours before the package index is refreshed.
    pub refresh_hours: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub accent: String,
    pub dim: String,
    pub warn: String,
    pub danger: String,
    pub ok: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            apps: AppsConfig::default(),
            safety: SafetyConfig::default(),
            aur: AurConfig::default(),
            theme: ThemeConfig::default(),
            keys: HashMap::new(),
        }
    }
}

impl Default for AppsConfig {
    fn default() -> Self {
        AppsConfig {
            merge_suffixes: crate::apps::DEFAULT_MERGE_SUFFIXES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            noise: crate::apps::DEFAULT_NOISE
                .iter()
                .map(|s| s.to_string())
                .collect(),
            appimage_dirs: Vec::new(),
        }
    }
}

impl Default for SafetyConfig {
    fn default() -> Self {
        SafetyConfig {
            also_protect: Vec::new(),
            dangerous_bytes: 500 * 1024 * 1024,
        }
    }
}

impl Default for AurConfig {
    fn default() -> Self {
        AurConfig {
            helper: String::new(),
            refresh_hours: 24,
        }
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        ThemeConfig {
            accent: "cyan".into(),
            dim: "darkgray".into(),
            warn: "yellow".into(),
            danger: "red".into(),
            ok: "green".into(),
        }
    }
}

/// Colours resolved from names, ready for the renderer.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub accent: Color,
    pub dim: Color,
    pub warn: Color,
    pub danger: Color,
    pub ok: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::from(&ThemeConfig::default())
    }
}

impl From<&ThemeConfig> for Theme {
    fn from(c: &ThemeConfig) -> Self {
        Theme {
            accent: parse_colour(&c.accent).unwrap_or(Color::Cyan),
            dim: parse_colour(&c.dim).unwrap_or(Color::DarkGray),
            warn: parse_colour(&c.warn).unwrap_or(Color::Yellow),
            danger: parse_colour(&c.danger).unwrap_or(Color::Red),
            ok: parse_colour(&c.ok).unwrap_or(Color::Green),
        }
    }
}

/// Parses a colour name or `#rrggbb`.
pub fn parse_colour(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    Some(match s.to_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "white" => Color::White,
        "reset" | "default" => Color::Reset,
        _ => return None,
    })
}

/// Something the user can bind a key to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    Help,
    Search,
    Remove,
    Undo,
    Files,
    Update,
    Refresh,
    NextView,
    PrevView,
    View(usize),
    ToggleOrphanMode,
    CleanOrphans,
}

impl Action {
    /// The name used in the config file.
    pub fn name(&self) -> String {
        match self {
            Action::Quit => "quit".into(),
            Action::Help => "help".into(),
            Action::Search => "search".into(),
            Action::Remove => "remove".into(),
            Action::Undo => "undo".into(),
            Action::Files => "files".into(),
            Action::Update => "update".into(),
            Action::Refresh => "refresh".into(),
            Action::NextView => "next_view".into(),
            Action::PrevView => "prev_view".into(),
            Action::View(n) => format!("view_{n}"),
            Action::ToggleOrphanMode => "toggle_orphan_mode".into(),
            Action::CleanOrphans => "clean_orphans".into(),
        }
    }

    pub fn from_name(name: &str) -> Option<Action> {
        Some(match name {
            "quit" => Action::Quit,
            "help" => Action::Help,
            "search" => Action::Search,
            "remove" => Action::Remove,
            "undo" => Action::Undo,
            "files" => Action::Files,
            "update" => Action::Update,
            "refresh" => Action::Refresh,
            "next_view" => Action::NextView,
            "prev_view" => Action::PrevView,
            "toggle_orphan_mode" => Action::ToggleOrphanMode,
            "clean_orphans" => Action::CleanOrphans,
            other => {
                let n: usize = other.strip_prefix("view_")?.parse().ok()?;
                Action::View(n)
            }
        })
    }
}

/// Key bindings, defaults plus whatever the config overrides.
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: Vec<(KeyCode, KeyModifiers, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        let mut bindings = vec![
            (KeyCode::Char('q'), KeyModifiers::NONE, Action::Quit),
            (KeyCode::Char('q'), KeyModifiers::CONTROL, Action::Quit),
            (KeyCode::Char('c'), KeyModifiers::CONTROL, Action::Quit),
            (KeyCode::F(1), KeyModifiers::NONE, Action::Help),
            (KeyCode::Char('f'), KeyModifiers::NONE, Action::Search),
            (KeyCode::Char('f'), KeyModifiers::CONTROL, Action::Search),
            (KeyCode::Delete, KeyModifiers::NONE, Action::Remove),
            (KeyCode::Char('z'), KeyModifiers::CONTROL, Action::Undo),
            (KeyCode::Char('l'), KeyModifiers::NONE, Action::Files),
            (KeyCode::Char('u'), KeyModifiers::NONE, Action::Update),
            (KeyCode::F(5), KeyModifiers::NONE, Action::Refresh),
            (KeyCode::Tab, KeyModifiers::NONE, Action::NextView),
            (KeyCode::BackTab, KeyModifiers::SHIFT, Action::PrevView),
            (KeyCode::BackTab, KeyModifiers::NONE, Action::PrevView),
            (KeyCode::Char(' '), KeyModifiers::NONE, Action::ToggleOrphanMode),
            (KeyCode::Char('c'), KeyModifiers::NONE, Action::CleanOrphans),
        ];
        for n in 1..=6 {
            bindings.push((
                KeyCode::Char(char::from_digit(n as u32, 10).unwrap()),
                KeyModifiers::NONE,
                Action::View(n),
            ));
            // F2-F6 stay as aliases for the first four views.
            if n <= 4 {
                bindings.push((KeyCode::F(n as u8 + 1), KeyModifiers::NONE, Action::View(n)));
            }
        }
        Keymap { bindings }
    }
}

impl Keymap {
    /// Applies the user's overrides on top of the defaults.
    ///
    /// An override *replaces* every default binding for that action, so
    /// rebinding `quit` to `x` does not leave `q` also quitting — which would
    /// be a surprising way to lose your place.
    pub fn with_overrides(overrides: &HashMap<String, String>) -> Self {
        let mut map = Keymap::default();
        for (name, key) in overrides {
            let Some(action) = Action::from_name(name) else {
                continue;
            };
            let Some((code, mods)) = parse_key(key) else {
                continue;
            };
            // Drop the action's old keys *and* whatever else held this key.
            // Without the second half, rebinding onto a key that already means
            // something silently does nothing: the earlier binding wins the
            // lookup and the user's line in the config has no visible effect.
            map.bindings.retain(|(c, m, a)| *a != action && !(*c == code && *m == mods));
            map.bindings.push((code, mods, action));
        }
        map
    }

    pub fn action_for(&self, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(c, m, _)| *c == code && *m == mods)
            .map(|(_, _, a)| *a)
    }

    /// The first key bound to an action, for the hint bar.
    pub fn key_for(&self, action: Action) -> Option<String> {
        self.bindings
            .iter()
            .find(|(_, _, a)| *a == action)
            .map(|(c, m, _)| describe_key(*c, *m))
    }
}

/// Parses `ctrl+f`, `F5`, `delete`, `q`.
pub fn parse_key(s: &str) -> Option<(KeyCode, KeyModifiers)> {
    let s = s.trim();
    let (mods, key) = match s.rsplit_once('+') {
        Some((m, k)) => {
            let mut mods = KeyModifiers::NONE;
            for part in m.split('+') {
                match part.trim().to_lowercase().as_str() {
                    "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
                    "shift" => mods |= KeyModifiers::SHIFT,
                    "alt" => mods |= KeyModifiers::ALT,
                    _ => return None,
                }
            }
            (mods, k.trim())
        }
        None => (KeyModifiers::NONE, s),
    };

    let code = match key.to_lowercase().as_str() {
        "delete" | "del" => KeyCode::Delete,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        other => {
            if let Some(n) = other.strip_prefix('f') {
                if let Ok(n) = n.parse::<u8>() {
                    return Some((KeyCode::F(n), mods));
                }
            }
            let mut chars = other.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        }
    };
    Some((code, mods))
}

fn describe_key(code: KeyCode, mods: KeyModifiers) -> String {
    let base = match code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Delete => "Del".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "Shift+Tab".to_string(),
        other => format!("{other:?}"),
    };
    if mods.contains(KeyModifiers::CONTROL) {
        format!("Ctrl+{base}")
    } else {
        base
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("apothiki").join("config.toml"))
    }

    /// Loads the config, falling back to defaults.
    ///
    /// Returns any error alongside the defaults rather than instead of them: a
    /// malformed config should be reported, not fatal.
    pub fn load() -> (Config, Option<String>) {
        let Some(path) = Config::path() else {
            return (Config::default(), None);
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (Config::default(), None)
            }
            Err(e) => return (Config::default(), Some(format!("{}: {e}", path.display()))),
        };
        match toml::from_str::<Config>(&text) {
            Ok(c) => (c, None),
            Err(e) => (
                Config::default(),
                Some(format!("{}: {e}", path.display())),
            ),
        }
    }

    pub fn keymap(&self) -> Keymap {
        Keymap::with_overrides(&self.keys)
    }

    pub fn theme(&self) -> Theme {
        Theme::from(&self.theme)
    }

    /// Writes a commented example, without overwriting an existing file.
    pub fn write_example() -> anyhow::Result<PathBuf> {
        let Some(path) = Config::path() else {
            anyhow::bail!("no config directory");
        };
        if path.exists() {
            anyhow::bail!("{} already exists", path.display());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, EXAMPLE)?;
        Ok(path)
    }
}

const EXAMPLE: &str = r#"# apothiki configuration. Every value here has a working default;
# delete anything you do not want to change.

[apps]
# Package suffixes folded into the application they belong to.
# gimp-help-en becomes part of GIMP rather than its own row.
merge_suffixes = ["docs", "doc", "data", "common", "icons", "themes", "i18n", "l10n", "help", "lang"]

# Desktop entries to hide. `*` is a wildcard.
noise = ["*-url-handler.desktop", "org.kde.kwin.*", "xterm.desktop"]

# Extra places to look for AppImages, on top of ~/AppImages, ~/Applications,
# ~/Downloads, ~/bin, ~/.local/bin and /opt.
appimage_dirs = []

[safety]
# Extra packages that may never be removed. Additive: there is deliberately no
# way to unprotect something the built-in denylist covers.
also_protect = []

# Removals freeing more than this are treated as dangerous (bytes).
dangerous_bytes = 524288000

[aur]
# Leave empty to detect paru, then yay, then pikaur.
helper = ""
refresh_hours = 24

[theme]
# Colour names or #rrggbb.
accent = "cyan"
dim = "darkgray"
warn = "yellow"
danger = "red"
ok = "green"

[keys]
# Rebinding an action replaces its default keys entirely.
# Modifiers: ctrl+, alt+, shift+. Names: delete, tab, enter, esc, space, F1-F12.
# quit = "q"
# search = "f"
# remove = "delete"
# files = "l"
# update = "u"
# undo = "ctrl+z"
# next_view = "tab"
# view_1 = "1"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_is_not_an_error() {
        let (config, error) = (Config::default(), None::<String>);
        assert!(error.is_none());
        assert!(!config.apps.merge_suffixes.is_empty());
    }

    #[test]
    fn a_broken_config_falls_back_rather_than_failing() {
        // Parsing is the part that can fail; the loader wraps it so a typo
        // never stops the program starting.
        assert!(toml::from_str::<Config>("[apps]\nmerge_suffixes = 3").is_err());
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.aur.refresh_hours, 24);
    }

    #[test]
    fn partial_config_keeps_defaults_for_everything_else() {
        let c: Config = toml::from_str("[aur]\nhelper = \"yay\"\n").unwrap();
        assert_eq!(c.aur.helper, "yay");
        assert_eq!(c.aur.refresh_hours, 24);
        assert!(!c.apps.noise.is_empty());
        assert_eq!(c.theme.accent, "cyan");
    }

    #[test]
    fn colours_parse_by_name_and_hex() {
        assert_eq!(parse_colour("cyan"), Some(Color::Cyan));
        assert_eq!(parse_colour("DarkGray"), Some(Color::DarkGray));
        assert_eq!(parse_colour("#ff8800"), Some(Color::Rgb(255, 136, 0)));
        assert_eq!(parse_colour("not-a-colour"), None);
        assert_eq!(parse_colour("#fff"), None);
    }

    #[test]
    fn keys_parse_with_and_without_modifiers() {
        assert_eq!(parse_key("q"), Some((KeyCode::Char('q'), KeyModifiers::NONE)));
        assert_eq!(
            parse_key("ctrl+f"),
            Some((KeyCode::Char('f'), KeyModifiers::CONTROL))
        );
        assert_eq!(parse_key("F5"), Some((KeyCode::F(5), KeyModifiers::NONE)));
        assert_eq!(parse_key("delete"), Some((KeyCode::Delete, KeyModifiers::NONE)));
        assert_eq!(parse_key("space"), Some((KeyCode::Char(' '), KeyModifiers::NONE)));
        assert_eq!(parse_key("nonsense"), None);
    }

    #[test]
    fn defaults_bind_the_documented_keys() {
        let map = Keymap::default();
        assert_eq!(
            map.action_for(KeyCode::Char('q'), KeyModifiers::NONE),
            Some(Action::Quit)
        );
        assert_eq!(
            map.action_for(KeyCode::Delete, KeyModifiers::NONE),
            Some(Action::Remove)
        );
        assert_eq!(
            map.action_for(KeyCode::Char('5'), KeyModifiers::NONE),
            Some(Action::View(5))
        );
    }

    #[test]
    fn an_override_replaces_the_default_rather_than_adding_to_it() {
        // Leaving the old key working would be a quiet way to keep quitting by
        // accident after deliberately moving the binding.
        let mut overrides = HashMap::new();
        overrides.insert("quit".to_string(), "x".to_string());
        let map = Keymap::with_overrides(&overrides);

        assert_eq!(
            map.action_for(KeyCode::Char('x'), KeyModifiers::NONE),
            Some(Action::Quit)
        );
        assert_eq!(map.action_for(KeyCode::Char('q'), KeyModifiers::NONE), None);
        // Ctrl+Q was a separate binding for the same action and goes too.
        assert_eq!(
            map.action_for(KeyCode::Char('q'), KeyModifiers::CONTROL),
            None
        );
    }

    #[test]
    fn rebinding_onto_a_taken_key_wins_it() {
        // F2 is a default alias for view 1. Asking for it explicitly must take
        // it, not be silently ignored because the default matched first.
        let mut overrides = HashMap::new();
        overrides.insert("files".to_string(), "F2".to_string());
        let map = Keymap::with_overrides(&overrides);

        assert_eq!(
            map.action_for(KeyCode::F(2), KeyModifiers::NONE),
            Some(Action::Files)
        );
        // The number row still reaches that view.
        assert_eq!(
            map.action_for(KeyCode::Char('1'), KeyModifiers::NONE),
            Some(Action::View(1))
        );
    }

    #[test]
    fn unknown_actions_and_keys_are_ignored_not_fatal() {
        let mut overrides = HashMap::new();
        overrides.insert("not_an_action".to_string(), "x".to_string());
        overrides.insert("quit".to_string(), "not-a-key".to_string());
        let map = Keymap::with_overrides(&overrides);
        // The bad rebinding was skipped, so the default still works.
        assert_eq!(
            map.action_for(KeyCode::Char('q'), KeyModifiers::NONE),
            Some(Action::Quit)
        );
    }

    #[test]
    fn the_example_config_is_valid() {
        // It ships as documentation, so it must actually parse.
        let parsed: Result<Config, _> = toml::from_str(EXAMPLE);
        assert!(parsed.is_ok(), "{:?}", parsed.err());
    }
}
