//! Removing the things pacman does not own: Flatpaks and AppImages (spec §12/M4).
//!
//! Both are removals, but almost nothing else is shared with the pacman path:
//!
//! - **Flatpak** has a real package manager of its own, so we drive it. System
//!   installations need privileges; user installations do not, and asking for a
//!   password when none is needed is its own kind of failure.
//! - **AppImage** has no manager at all. Removal means deleting files we
//!   identified ourselves, which is the only place in this program that deletes
//!   anything directly — so it is fenced in hard (see [`is_safe_target`]).

use std::path::{Path, PathBuf};

/// A Flatpak application to uninstall.
#[derive(Debug, Clone)]
pub struct FlatpakRemoval {
    pub id: String,
    pub name: String,
    /// System installations are shared between users and need root; user
    /// installations live in `~/.local/share/flatpak` and do not.
    pub system: bool,
    /// Also drop runtimes nothing needs any more.
    pub remove_unused: bool,
}

impl FlatpakRemoval {
    pub fn args(&self) -> Vec<String> {
        vec![
            "uninstall".to_string(),
            if self.system { "--system" } else { "--user" }.to_string(),
            "--assumeyes".to_string(),
            self.id.clone(),
        ]
    }

    pub fn unused_args(&self) -> Vec<String> {
        vec![
            "uninstall".to_string(),
            "--unused".to_string(),
            if self.system { "--system" } else { "--user" }.to_string(),
            "--assumeyes".to_string(),
        ]
    }

    /// Whether this needs to run through sudo.
    pub fn needs_privileges(&self) -> bool {
        self.system
    }

    pub fn command_line(&self) -> String {
        let prefix = if self.system { "sudo " } else { "" };
        format!("{prefix}flatpak {}", self.args().join(" "))
    }
}

/// An AppImage to delete, component by component.
///
/// Each part is a separate decision because they carry different risk: the
/// bundle is the application, the desktop entry and icon are integration
/// leftovers, and the data directory holds work the user may want to keep.
#[derive(Debug, Clone)]
pub struct AppImageRemoval {
    pub name: String,
    pub bundle: PathBuf,
    pub desktop_entry: Option<PathBuf>,
    pub icon: Option<PathBuf>,
    /// Guessed user data directories. Off by default — these are the only
    /// paths here we are not certain belong to this application.
    pub user_data: Vec<PathBuf>,
    pub remove_desktop: bool,
    pub remove_icon: bool,
    pub remove_data: bool,
}

impl AppImageRemoval {
    /// Everything that would be deleted, in order.
    pub fn targets(&self) -> Vec<PathBuf> {
        let mut out = vec![self.bundle.clone()];
        if self.remove_desktop {
            out.extend(self.desktop_entry.clone());
        }
        if self.remove_icon {
            out.extend(self.icon.clone());
        }
        if self.remove_data {
            out.extend(self.user_data.iter().cloned());
        }
        out
    }

    pub fn command_line(&self) -> String {
        format!("rm -r {}", self.targets().len())
    }
}

/// Whether a path may be deleted by this program.
///
/// **This is the fence.** Every other mutation in the program is handed to
/// pacman or flatpak, which have their own rules about what they will touch.
/// AppImage removal is the one case where we delete files ourselves, and the
/// paths come from a `.desktop` file we parsed — so a malformed or hostile
/// entry must not be able to point us at `/` or `~/.ssh`.
///
/// The rule: inside the user's home, not the home directory itself, and not one
/// of the standard directories a user would be horrified to lose.
pub fn is_safe_target(path: &Path, home: &Path) -> bool {
    // `..` is rejected outright rather than resolved. `Path::starts_with` is
    // purely lexical, so `/home/you/../../etc` "starts with" the home directory
    // and would otherwise pass the check below. Every path here is one we built
    // from a desktop entry, so a parent-directory component is never legitimate
    // and is far more likely to be an attempt to escape.
    if path.components().any(|c| c == std::path::Component::ParentDir) {
        return false;
    }

    // Resolve symlinks so a link inside home cannot point outside it.
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if resolved.components().any(|c| c == std::path::Component::ParentDir) {
        return false;
    }

    if !resolved.starts_with(home) || resolved == home {
        return false;
    }

    // Never a top-level directory of the home itself.
    let protected = [
        ".ssh",
        ".gnupg",
        ".config",
        ".local",
        ".cache",
        "Documents",
        "Desktop",
        "Downloads",
        "Pictures",
        "Videos",
        "Music",
    ];
    if let Ok(rest) = resolved.strip_prefix(home) {
        let depth = rest.components().count();
        if depth == 0 {
            return false;
        }
        if depth == 1 {
            let first = rest.components().next().and_then(|c| c.as_os_str().to_str());
            // A single component under home is only ever a whole directory such
            // as ~/Documents, or a stray file. Neither is ours to delete.
            if first.is_some_and(|f| protected.contains(&f)) {
                return false;
            }
        }
    }

    true
}

/// Deletes the AppImage's files, reporting each outcome.
///
/// Returns the log rather than printing it, so the caller can show every line
/// and the user can see exactly what happened.
pub fn delete_appimage(plan: &AppImageRemoval, home: &Path) -> (bool, Vec<String>) {
    let mut log = Vec::new();
    let mut ok = true;

    for target in plan.targets() {
        if !is_safe_target(&target, home) {
            log.push(format!("refused (outside your home): {}", target.display()));
            ok = false;
            continue;
        }
        let result = if target.is_dir() {
            std::fs::remove_dir_all(&target)
        } else {
            std::fs::remove_file(&target)
        };
        match result {
            Ok(()) => log.push(format!("removed {}", target.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log.push(format!("already gone: {}", target.display()));
            }
            Err(e) => {
                log.push(format!("could not remove {}: {e}", target.display()));
                ok = false;
            }
        }
    }

    (ok, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/tester")
    }

    #[test]
    fn flatpak_scope_selects_the_right_flag() {
        let system = FlatpakRemoval {
            id: "app.zen_browser.zen".into(),
            name: "Zen".into(),
            system: true,
            remove_unused: false,
        };
        assert!(system.args().contains(&"--system".to_string()));
        assert!(system.needs_privileges());
        assert!(system.command_line().starts_with("sudo "));

        let user = FlatpakRemoval {
            system: false,
            ..system.clone()
        };
        assert!(user.args().contains(&"--user".to_string()));
        assert!(!user.needs_privileges());
        // Asking for a password that is not needed is its own failure.
        assert!(!user.command_line().starts_with("sudo "));
    }

    #[test]
    fn appimage_components_are_individually_optional() {
        let plan = AppImageRemoval {
            name: "Obsidian".into(),
            bundle: home().join("AppImages/obsidian.appimage"),
            desktop_entry: Some(home().join(".local/share/applications/obsidian.desktop")),
            icon: Some(home().join("AppImages/.icons/obsidian")),
            user_data: vec![home().join(".config/obsidian")],
            remove_desktop: true,
            remove_icon: true,
            remove_data: false,
        };
        // Data is off by default: it is the part we are least sure about and
        // the part the user would most regret losing.
        assert_eq!(plan.targets().len(), 3);
        assert!(!plan.targets().iter().any(|p| p.ends_with(".config/obsidian")));

        let with_data = AppImageRemoval {
            remove_data: true,
            ..plan.clone()
        };
        assert_eq!(with_data.targets().len(), 4);

        let bundle_only = AppImageRemoval {
            remove_desktop: false,
            remove_icon: false,
            ..plan
        };
        assert_eq!(bundle_only.targets().len(), 1);
    }

    #[test]
    fn deletion_is_fenced_to_the_home_directory() {
        let h = home();
        assert!(is_safe_target(&h.join("AppImages/thing.appimage"), &h));
        assert!(is_safe_target(&h.join(".config/obsidian"), &h));

        // Outside home entirely.
        assert!(!is_safe_target(Path::new("/usr/bin/pacman"), &h));
        assert!(!is_safe_target(Path::new("/"), &h));
        assert!(!is_safe_target(Path::new("/etc/passwd"), &h));
        // The home directory itself.
        assert!(!is_safe_target(&h, &h));
        // Another user's home.
        assert!(!is_safe_target(Path::new("/home/someone-else/x"), &h));
    }

    #[test]
    fn whole_standard_directories_are_never_targets() {
        let h = home();
        for dir in [".ssh", ".gnupg", ".config", ".local", "Documents", "Downloads"] {
            assert!(
                !is_safe_target(&h.join(dir), &h),
                "{dir} must never be deletable"
            );
        }
        // But a named directory *inside* one of them is fine.
        assert!(is_safe_target(&h.join(".config/obsidian"), &h));
    }

    #[test]
    fn a_traversing_path_cannot_escape() {
        // `Path::starts_with` is lexical, so without an explicit check this
        // path "starts with" the home directory and passes.
        let h = home();
        assert!(!is_safe_target(Path::new("/home/tester/../../etc"), &h));
        assert!(!is_safe_target(Path::new("/home/tester/AppImages/../../.ssh"), &h));
        assert!(!is_safe_target(Path::new("/home/tester/.."), &h));
    }
}
