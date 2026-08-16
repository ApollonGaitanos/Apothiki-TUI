//! Layer 4: AppImages (spec §4.2).
//!
//! **No registry exists anywhere.** There is no database, no manifest, and no
//! CLI to ask — an AppImage is just an executable file someone downloaded. All
//! discovery is therefore filesystem work, and any list of "the usual
//! directories" is a guess.
//!
//! That guess is unreliable in practice: the spec's suggested directories
//! (`~/Applications`, `~/.local/bin`, `~/Downloads`, `~/bin`, `/opt`) find
//! **zero** AppImages on the dev machine, which keeps all four of its AppImages
//! in `~/AppImages`. So the primary discovery route here is not directory
//! scanning at all but **following `Exec=` out of desktop entries that no
//! package owns** — that finds them wherever the user put them. Directory
//! scanning is kept as a secondary pass, to catch AppImages that were never
//! integrated into a launcher.
//!
//! AppImages are self-contained: they have **no dependency graph**. The UI must
//! say "self-contained bundle" rather than render an empty dependency list,
//! which reads as a bug (spec §13.13).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AppImage {
    pub path: PathBuf,
    /// Filename without the extension — the only name available without
    /// extracting the bundle, which v1 deliberately does not do.
    pub file_stem: String,
    /// The integrated desktop entry, when one exists.
    pub desktop_id: Option<String>,
}

/// Directories worth scanning for un-integrated AppImages.
///
/// Deliberately includes `~/AppImages`, which the spec omits and which is where
/// this machine actually keeps them. Belongs in config, not code.
pub fn default_dirs() -> Vec<PathBuf> {
    dirs_with(&[])
}

/// The default directories plus any the user configured.
pub fn dirs_with(extra: &[String]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = extra.iter().map(PathBuf::from).collect();
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        out.push(PathBuf::from("/opt"));
        return out;
    };
    out.extend(
        ["AppImages", "Applications", "Downloads", "bin", ".local/bin"]
            .iter()
            .map(|d| home.join(d)),
    );
    out.push(PathBuf::from("/opt"));
    out
}

fn is_appimage(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("appimage"))
}

/// True if the file has any execute bit set.
///
/// A non-executable `.AppImage` is a download the user never ran; listing it as
/// an installed application would be wrong.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Extracts the program path from a desktop `Exec=` line.
///
/// Handles the quoting and the `%U`/`%f` field codes that desktop entries use.
pub fn exec_target(exec: &str) -> Option<PathBuf> {
    let mut token = String::new();
    let mut chars = exec.trim().chars().peekable();

    // A quoted first token may contain spaces.
    if matches!(chars.peek(), Some('"') | Some('\'')) {
        let quote = chars.next()?;
        for c in chars.by_ref() {
            if c == quote {
                break;
            }
            token.push(c);
        }
    } else {
        for c in chars.by_ref() {
            if c == ' ' {
                break;
            }
            token.push(c);
        }
    }

    // `env VAR=x /path/to/app` — skip the wrapper and its assignments.
    if token == "env" || token.ends_with("/env") {
        let rest: String = chars.collect();
        let remainder = rest
            .split_whitespace()
            .find(|t| !t.contains('=') && !t.is_empty())?;
        return Some(PathBuf::from(remainder));
    }

    (!token.is_empty() && !token.starts_with('%')).then(|| PathBuf::from(token))
}

/// Finds AppImages referenced by desktop entries that no package owns.
///
/// This is the reliable route: it works wherever the user keeps them.
pub fn from_desktop_entries<'a>(
    unowned: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<AppImage> {
    let mut out = Vec::new();
    for (desktop_id, exec) in unowned {
        let Some(target) = exec_target(exec) else {
            continue;
        };
        if !is_appimage(&target) || !is_executable(&target) {
            continue;
        }
        out.push(AppImage {
            file_stem: stem_of(&target),
            desktop_id: Some(desktop_id.to_string()),
            path: target,
        });
    }
    out
}

/// Scans directories for AppImages, including ones never integrated into a
/// launcher. Non-recursive beyond one level: these directories can be large
/// (`~/Downloads`), and AppImages are not filed in subtrees.
pub fn scan_dirs(dirs: &[PathBuf]) -> Vec<AppImage> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if is_appimage(&path) && is_executable(&path) {
                out.push(AppImage {
                    file_stem: stem_of(&path),
                    desktop_id: None,
                    path,
                });
            }
        }
    }
    out
}

fn stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Merges both discovery routes, preferring entries that carry a desktop id.
pub fn discover<'a>(
    unowned: impl IntoIterator<Item = (&'a str, &'a str)>,
    dirs: &[PathBuf],
) -> Vec<AppImage> {
    let mut by_path: BTreeMap<PathBuf, AppImage> = BTreeMap::new();

    for img in scan_dirs(dirs) {
        by_path.insert(canonical(&img.path), img);
    }
    // Desktop-derived entries win: they carry a name and a launcher binding.
    for img in from_desktop_entries(unowned) {
        by_path.insert(canonical(&img.path), img);
    }

    by_path.into_values().collect()
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_program_from_exec_lines() {
        assert_eq!(
            exec_target("/home/apo/AppImages/helium.appimage %U"),
            Some(PathBuf::from("/home/apo/AppImages/helium.appimage"))
        );
        assert_eq!(
            exec_target("\"/home/apo/.local/bin/claude\" --flag"),
            Some(PathBuf::from("/home/apo/.local/bin/claude"))
        );
        assert_eq!(exec_target("steam steam://rungameid/286160"), Some(PathBuf::from("steam")));
    }

    #[test]
    fn sees_through_env_wrappers() {
        // Nine user entries on the dev machine launch via `env`.
        assert_eq!(
            exec_target("env FOO=1 BAR=2 /home/apo/AppImages/session.appimage %U"),
            Some(PathBuf::from("/home/apo/AppImages/session.appimage"))
        );
        assert_eq!(
            exec_target("/usr/bin/env /opt/thing.AppImage"),
            Some(PathBuf::from("/opt/thing.AppImage"))
        );
    }

    #[test]
    fn ignores_empty_and_field_code_only_execs() {
        assert_eq!(exec_target(""), None);
        assert_eq!(exec_target("%U"), None);
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        assert!(is_appimage(Path::new("/x/Foo.AppImage")));
        assert!(is_appimage(Path::new("/x/foo.appimage")));
        assert!(!is_appimage(Path::new("/x/foo.deb")));
        assert!(!is_appimage(Path::new("/x/appimage")));
    }

    #[test]
    fn default_dirs_include_the_one_this_machine_uses() {
        // The spec's list omits ~/AppImages, where all four AppImages here live.
        let dirs = default_dirs();
        assert!(
            dirs.iter().any(|d| d.ends_with("AppImages")),
            "{dirs:?}"
        );
    }

    #[test]
    fn non_executable_downloads_are_not_installed_apps() {
        let dir = std::env::temp_dir().join(format!("apothiki-ai-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("thing.AppImage");
        std::fs::write(&f, b"not really").unwrap();

        assert!(scan_dirs(&[dir.clone()]).is_empty(), "no execute bit yet");

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(scan_dirs(&[dir.clone()]).len(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
