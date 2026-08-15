//! Privileged execution: pre-authenticate, then stream (spec §7.4, option C).
//!
//! The user's requirement was that escalation never leaves the TUI and never
//! costs more than one password entry: *select action → prompt if needed →
//! done*. That rules out suspend-and-hand-off. It does **not** mean parsing a
//! password prompt out of a command's output, which is the classic foot-gun.
//!
//! Instead the two concerns are separated:
//!
//! 1. `sudo -n -v` asks whether we are already authenticated. Usually yes, and
//!    nothing is shown.
//! 2. If not, a masked TUI field collects the password once and feeds it to
//!    `sudo -S -v`, which **validates only** — it runs no command. The buffer is
//!    zeroed immediately; it is never logged, never placed in argv, never
//!    written to disk.
//! 3. Everything afterwards runs `sudo -n …`, whose output can be streamed
//!    without any prompt ever appearing in it.
//!
//! Nothing here executes without an explicit confirmed request from the UI, and
//! every plan is checked against a real `pacman --print` dry-run first.
//!
//! **No shell is ever involved.** Every command is spawned as
//! `Command::new(program).args([...])`, which is `execvp` directly — no `sh -c`,
//! no word splitting, no globbing, no quoting rules. The user's login shell is
//! therefore irrelevant: fish, bash and zsh behave identically here, and
//! arguments containing spaces (a snapshot description, a cache path) arrive as
//! single argv entries without escaping.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

/// A password held only as long as it takes to hand to sudo.
///
/// `Drop` overwrites the bytes. Rust may still have moved the buffer during
/// reallocation, so this is a best effort rather than a guarantee — which is
/// exactly why the password is used once for validation and never retained.
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Self {
        Secret(s)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Overwrite in place before the allocation is released.
        unsafe {
            for b in self.0.as_bytes_mut() {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let a password reach a log or a panic message.
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
    /// `sudo -n -v` succeeded: a cached timestamp or NOPASSWD. No prompt needed.
    Ready,
    /// A password is required.
    NeedsPassword,
    /// sudo is not usable at all.
    Unavailable,
}

/// Asks sudo whether we are already authenticated, without prompting.
pub fn check_auth() -> AuthState {
    match Command::new("sudo")
        .args(["-n", "-v"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(s) if s.success() => AuthState::Ready,
        Ok(_) => AuthState::NeedsPassword,
        Err(_) => AuthState::Unavailable,
    }
}

/// Validates a password by refreshing the sudo timestamp. Runs no command.
///
/// `-S` reads from stdin, `-v` validates only, and `-p ''` suppresses the
/// prompt text we would otherwise have to read past.
pub fn authenticate(secret: &Secret) -> Result<(), String> {
    let mut child = Command::new("sudo")
        .args(["-S", "-p", "", "-v"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // Captured, not discarded: sudo explains its own refusals, and
        // reporting "authentication failed" when it actually said something
        // specific leaves the user with nothing to act on.
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run sudo: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        // The newline is what submits it; without it sudo waits forever.
        let _ = stdin.write_all(secret.0.as_bytes());
        let _ = stdin.write_all(b"\n");
        // Closing stdin stops sudo retrying against an empty stream.
        drop(stdin);
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("sudo did not complete: {e}"))?;

    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = stderr
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.contains("try again"))
        .unwrap_or("incorrect password");
    Err(detail.to_string())
}

/// A line of output from a running operation.
#[derive(Debug, Clone)]
pub enum Output {
    Line(String),
    Finished { success: bool, code: Option<i32> },
    Failed(String),
}

/// The result of a privileged command.
pub struct RunResult {
    pub success: bool,
    pub code: Option<i32>,
    /// Every line produced, for callers that need to read a value back out.
    pub lines: Vec<String>,
}

/// Runs a privileged command, streaming its output to `tx` as it arrives.
///
/// **Streams rather than collects.** The obvious shape — run the command into a
/// local channel, then forward that channel — does not stream at all: the run
/// completes before forwarding begins, so the user watches a frozen dialog and
/// then sees the whole log at once, exactly when it has stopped being useful.
/// Lines go straight to the caller's channel here, and the summary is returned
/// rather than sent, so the caller decides when the operation is "finished".
///
/// Uses `sudo -n`, which never prompts: authentication has already happened, so
/// a failure here means the timestamp expired and the caller should re-auth
/// rather than hang waiting for input nobody can see.
pub fn run_privileged(program: &str, args: &[String], tx: &Sender<Output>) -> RunResult {
    let mut full: Vec<String> = vec!["-n".into(), program.into()];
    full.extend(args.iter().cloned());

    let child = Command::new("sudo")
        .args(&full)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("could not start sudo: {e}");
            let _ = tx.send(Output::Failed(msg.clone()));
            return RunResult {
                success: false,
                code: None,
                lines: vec![msg],
            };
        }
    };

    let mut lines: Vec<String> = Vec::new();

    // stderr is drained on its own thread. Reading the two streams in sequence
    // deadlocks as soon as one fills its pipe buffer while we are blocked on
    // the other — and pacman writes its progress to stderr.
    let err_handle = child.stderr.take().map(|err| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut collected = Vec::new();
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = tx.send(Output::Line(line.clone()));
                collected.push(line);
            }
            collected
        })
    });

    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(Output::Line(line.clone()));
            lines.push(line);
        }
    }
    if let Some(h) = err_handle {
        if let Ok(collected) = h.join() {
            lines.extend(collected);
        }
    }

    match child.wait() {
        Ok(status) => RunResult {
            success: status.success(),
            code: status.code(),
            lines,
        },
        Err(e) => {
            let _ = tx.send(Output::Failed(e.to_string()));
            RunResult {
                success: false,
                code: None,
                lines,
            }
        }
    }
}

/// Runs a command as the current user, streaming its output.
///
/// AUR helpers must not run under sudo — they refuse to, and rightly: makepkg
/// compiles untrusted source, and doing that as root hands a malicious PKGBUILD
/// the machine. The helper escalates by itself for the final install, which
/// does not prompt because the sudo timestamp is already warm.
pub fn run_unprivileged(program: &str, args: &[String], tx: &Sender<Output>) -> RunResult {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("could not start {program}: {e}");
            let _ = tx.send(Output::Failed(msg.clone()));
            return RunResult {
                success: false,
                code: None,
                lines: vec![msg],
            };
        }
    };

    let mut lines: Vec<String> = Vec::new();
    let err_handle = child.stderr.take().map(|err| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut collected = Vec::new();
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = tx.send(Output::Line(line.clone()));
                collected.push(line);
            }
            collected
        })
    });
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(Output::Line(line.clone()));
            lines.push(line);
        }
    }
    if let Some(h) = err_handle {
        if let Ok(collected) = h.join() {
            lines.extend(collected);
        }
    }

    match child.wait() {
        Ok(status) => RunResult {
            success: status.success(),
            code: status.code(),
            lines,
        },
        Err(e) => {
            let _ = tx.send(Output::Failed(e.to_string()));
            RunResult { success: false, code: None, lines }
        }
    }
}

/// Runs `pacman --print` and returns the packages it would remove.
///
/// **This is the gate before any real removal.** Our in-process simulation is
/// fast and, across all 1656 packages on the dev machine, exact — but "verified
/// yesterday" is not a licence to delete on our own say-so. Read-only, needs no
/// privileges (spec §5.2).
pub fn dry_run(args: &[String]) -> anyhow::Result<Vec<String>> {
    let out = Command::new("pacman").args(args).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Compares our simulated removal set against pacman's own answer.
///
/// Returns the discrepancies. A non-empty result must abort the operation: it
/// means the tool's model of the system is wrong, and the one thing worse than
/// refusing to remove something is removing something else.
pub fn reconcile(ours: &[String], theirs: &[String], known: &[String]) -> Vec<String> {
    let strip = |line: &str| -> String {
        let mut cut = line.len();
        while let Some(i) = line[..cut].rfind('-') {
            if known.iter().any(|k| k == &line[..i]) {
                return line[..i].to_string();
            }
            cut = i;
        }
        line.to_string()
    };

    let theirs: std::collections::BTreeSet<String> = theirs.iter().map(|l| strip(l)).collect();
    let ours: std::collections::BTreeSet<String> = ours.iter().cloned().collect();

    let mut diff: Vec<String> = theirs
        .difference(&ours)
        .map(|p| format!("pacman would also remove {p}"))
        .collect();
    diff.extend(
        ours.difference(&theirs)
            .map(|p| format!("we expected {p} to be removed but pacman would not")),
    );
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_never_printed() {
        let s = Secret::new("hunter2".into());
        let shown = format!("{s:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn identical_sets_reconcile_cleanly() {
        let known = vec!["godot".to_string(), "embree".to_string()];
        let ours = vec!["godot".to_string(), "embree".to_string()];
        let theirs = vec!["godot-4.7.1-1.1".to_string(), "embree-4.4.1-1.1".to_string()];
        assert!(reconcile(&ours, &theirs, &known).is_empty());
    }

    #[test]
    fn a_package_pacman_would_also_take_is_reported() {
        let known = vec!["godot".to_string(), "extra-pkg".to_string()];
        let ours = vec!["godot".to_string()];
        let theirs = vec!["godot-4.7.1-1.1".to_string(), "extra-pkg-1.0-1".to_string()];
        let diff = reconcile(&ours, &theirs, &known);
        assert_eq!(diff.len(), 1);
        assert!(diff[0].contains("extra-pkg"), "{diff:?}");
    }

    #[test]
    fn a_package_we_expected_but_pacman_would_not_take_is_reported() {
        let known = vec!["godot".to_string(), "phantom".to_string()];
        let ours = vec!["godot".to_string(), "phantom".to_string()];
        let theirs = vec!["godot-4.7.1-1.1".to_string()];
        let diff = reconcile(&ours, &theirs, &known);
        assert_eq!(diff.len(), 1);
        assert!(diff[0].contains("phantom"), "{diff:?}");
    }

    #[test]
    fn versions_with_hyphens_strip_correctly() {
        let known = vec!["ca-certificates-utils".to_string()];
        let theirs = vec!["ca-certificates-utils-20240618-1".to_string()];
        let ours = vec!["ca-certificates-utils".to_string()];
        assert!(reconcile(&ours, &theirs, &known).is_empty());
    }
}
