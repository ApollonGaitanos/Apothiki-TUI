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

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};

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

/// Which of a command's streams a message came from.
///
/// Tracked because prompts and progress live on different streams and must not
/// erase each other: pacman writes `:: Proceed? [Y/n] ` to stdout as an
/// unterminated fragment while writing status lines to stderr, and a single
/// shared "current fragment" means the next stderr line deletes the question
/// the user is being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// A line of output from a running operation.
#[derive(Debug, Clone)]
pub enum Output {
    Line(Stream, String),
    /// The current incomplete line.
    ///
    /// **Prompts arrive this way.** `:: Proceed with installation? [Y/n] ` has
    /// no trailing newline, so a reader that only emits complete lines never
    /// shows it: the user sees nothing, the program waits for an answer to a
    /// question it never displayed, and the operation appears to hang.
    Partial(Stream, String),
    Finished { success: bool, code: Option<i32> },
    Failed(String),
}

impl Output {
    /// A message from us rather than from the command.
    pub fn info(text: impl Into<String>) -> Output {
        Output::Line(Stream::Stdout, text.into())
    }
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
    run_inner(program, args, true, None, tx)
}

/// A non-interactive command whose pid is published, so Ctrl+C can stop it.
///
/// Every operation should be interruptible, not only the ones that ask
/// questions: a removal cascading through two hundred packages is exactly as
/// trapping as a build.
pub fn run_privileged_tracked(
    program: &str,
    args: &[String],
    pids: &PidSlot,
    tx: &Sender<Output>,
) -> RunResult {
    run_tracked(program, args, true, None, Some(pids), tx)
}

pub fn run_unprivileged_tracked(
    program: &str,
    args: &[String],
    pids: &PidSlot,
    tx: &Sender<Output>,
) -> RunResult {
    run_tracked(program, args, false, None, Some(pids), tx)
}

/// The stdin of whichever command is currently running.
///
/// An operation can be several commands in sequence — a repository upgrade,
/// then an AUR rebuild — and the user has one place to type. A per-command
/// channel cannot express that: the first command's writer thread owns the
/// receiver and blocks on it forever, so the second command gets nothing and
/// falls back to `--noconfirm`, which is what made `paru` abort on a conflict.
///
/// So the answers channel is drained by a single writer for the whole
/// operation, and each command in turn publishes its stdin here.
pub type StdinSlot = std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>;

pub fn stdin_slot() -> StdinSlot {
    Default::default()
}

/// The process id of whichever command is currently running, so it can be
/// interrupted.
///
/// Without this a long operation is a trap: the dialog takes every key, the
/// command may run for many minutes, and the only way out is killing the whole
/// program — which orphans a live pacman transaction holding the database lock.
pub type PidSlot = std::sync::Arc<std::sync::Mutex<Option<u32>>>;

pub fn pid_slot() -> PidSlot {
    Default::default()
}

/// Sends SIGINT to the running command, as pressing Ctrl+C in a shell would.
///
/// SIGINT rather than SIGKILL deliberately: pacman handles it, finishing
/// whatever file operation is in flight and unwinding cleanly. Killing it
/// outright is how a half-written package and a stale lock happen.
pub fn interrupt(pid: &PidSlot) -> bool {
    let Ok(guard) = pid.lock() else { return false };
    let Some(pid) = *guard else { return false };

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGINT: i32 = 2;
    // SAFETY: kill on a pid we spawned; a dead pid simply returns an error.
    unsafe { kill(pid as i32, SIGINT) == 0 }
}

/// Starts the one writer thread that serves every command in an operation.
pub fn spawn_answer_writer(rx: Receiver<String>, slot: StdinSlot) {
    std::thread::spawn(move || {
        for line in rx {
            let Ok(mut guard) = slot.lock() else { return };
            if let Some(stdin) = guard.as_mut() {
                if stdin.write_all(line.as_bytes()).is_err()
                    || stdin.write_all(b"\n").is_err()
                    || stdin.flush().is_err()
                {
                    // The command exited between the prompt and the answer;
                    // the next one will publish a fresh stdin.
                    *guard = None;
                }
            }
        }
    });
}

/// As `run_privileged`, but with a channel feeding the command's stdin.
///
/// This is what makes `pacman -Syu` usable without `--noconfirm`: pacman asks
/// about replacements, providers and conflicts, and with `--noconfirm` it
/// answers each with the *default* — which for a conflict is "no", aborting the
/// whole transaction. Letting the user answer is not a convenience, it is the
/// difference between an upgrade that completes and one that cannot.
pub fn run_privileged_interactive(
    program: &str,
    args: &[String],
    slot: &StdinSlot,
    pids: &PidSlot,
    tx: &Sender<Output>,
) -> RunResult {
    run_tracked(program, args, true, Some(slot), Some(pids), tx)
}

pub fn run_unprivileged_interactive(
    program: &str,
    args: &[String],
    slot: &StdinSlot,
    pids: &PidSlot,
    tx: &Sender<Output>,
) -> RunResult {
    run_tracked(program, args, false, Some(slot), Some(pids), tx)
}

/// Streams a child's output, optionally feeding it input.
fn run_inner(
    program: &str,
    args: &[String],
    privileged: bool,
    slot: Option<&StdinSlot>,
    tx: &Sender<Output>,
) -> RunResult {
    run_tracked(program, args, privileged, slot, None, tx)
}

/// As `run_inner`, also publishing the child's pid so it can be interrupted.
fn run_tracked(
    program: &str,
    args: &[String],
    privileged: bool,
    slot: Option<&StdinSlot>,
    pids: Option<&PidSlot>,
    tx: &Sender<Output>,
) -> RunResult {
    let (cmd, full): (&str, Vec<String>) = if privileged {
        let mut v: Vec<String> = vec!["-n".into(), program.into()];
        v.extend(args.iter().cloned());
        ("sudo", v)
    } else {
        (program, args.to_vec())
    };

    let child = Command::new(cmd)
        .args(&full)
        .stdin(if slot.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("could not start {cmd}: {e}");
            let _ = tx.send(Output::Failed(msg.clone()));
            return RunResult {
                success: false,
                code: None,
                lines: vec![msg],
            };
        }
    };

    if let Some(pids) = pids {
        if let Ok(mut guard) = pids.lock() {
            *guard = Some(child.id());
        }
    }

    // Publish this command's stdin for the operation's writer thread.
    if let (Some(slot), Some(stdin)) = (slot, child.stdin.take()) {
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(stdin);
        }
    }

    let mut lines: Vec<String> = Vec::new();

    // stderr is drained on its own thread. Reading the two streams in sequence
    // deadlocks as soon as one fills its pipe buffer while we are blocked on
    // the other — and pacman writes its progress to stderr.
    let err_handle = child.stderr.take().map(|err| {
        let tx = tx.clone();
        std::thread::spawn(move || pump(err, Stream::Stderr, &tx))
    });

    if let Some(out) = child.stdout.take() {
        lines.extend(pump(out, Stream::Stdout, tx));
    }
    if let Some(h) = err_handle {
        if let Ok(collected) = h.join() {
            lines.extend(collected);
        }
    }

    // Closing stdin lets a command that is still reading finish, and stops the
    // next command inheriting a stale handle.
    if let Some(slot) = slot {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
    if let Some(pids) = pids {
        if let Ok(mut guard) = pids.lock() {
            *guard = None;
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

/// Reads a stream byte-wise, emitting complete lines and the trailing fragment.
///
/// Byte-wise rather than by line because a prompt is a fragment: it ends in a
/// space, not a newline, and waiting for one means never showing the question.
fn pump(stream: impl std::io::Read, which: Stream, tx: &Sender<Output>) -> Vec<String> {
    use std::io::Read;

    let mut reader = std::io::BufReader::new(stream);
    let mut buf = [0u8; 4096];
    let mut pending = String::new();
    let mut lines = Vec::new();

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.push_str(&String::from_utf8_lossy(&buf[..n]));

        while let Some(i) = pending.find('\n') {
            let line: String = pending.drain(..=i).collect();
            let line = line.trim_end_matches(['\n', '\r']).to_string();
            let _ = tx.send(Output::Line(which, line.clone()));
            lines.push(line);
        }
        if !pending.is_empty() {
            // Carriage returns are progress-bar redraws; only the last matters.
            let shown = pending.rsplit('\r').next().unwrap_or(&pending).to_string();
            let _ = tx.send(Output::Partial(which, shown));
        }
    }

    if !pending.is_empty() {
        let line = pending.trim_end().to_string();
        let _ = tx.send(Output::Line(which, line.clone()));
        lines.push(line);
    }
    lines
}

/// Runs a command as the current user, streaming its output.
///
/// AUR helpers must not run under sudo — they refuse to, and rightly: makepkg
/// compiles untrusted source, and doing that as root hands a malicious PKGBUILD
/// the machine. The helper escalates by itself for the final install, which
/// does not prompt because the sudo timestamp is already warm.
pub fn run_unprivileged(program: &str, args: &[String], tx: &Sender<Output>) -> RunResult {
    run_inner(program, args, false, None, tx)
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

#[cfg(test)]
mod interactive_tests {
    use super::*;

    /// A prompt with no trailing newline must reach the caller as `Partial`.
    ///
    /// This is the whole reason the reader is byte-wise. With a line-based
    /// reader this assertion never fires: the question is never displayed and
    /// the operation simply appears to hang.
    #[test]
    fn a_prompt_without_a_newline_is_delivered_and_answerable() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (itx, irx) = std::sync::mpsc::channel();

        // Answered from another thread, after a pause. `run_inner` blocks until
        // the child exits, so answering afterwards deadlocks; answering
        // instantly is no better a test, because the prompt and the echo then
        // arrive in a single read and the partial state is never observed. A
        // real user takes a moment, and that moment is the thing under test.
        let slot = stdin_slot();
        spawn_answer_writer(irx, slot.clone());
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let _ = itx.send("yes".to_string());
        });

        // `sh` is the subject under test, not a way to run our own commands:
        // it is the cheapest program that reproduces pacman's prompt shape.
        let result = run_inner(
            "sh",
            &[
                "-c".to_string(),
                "printf ':: Proceed? [Y/n] '; read a; echo \"got:$a\"".to_string(),
            ],
            false,
            Some(&slot),
            &tx,
        );
        assert!(result.success);

        let msgs: Vec<Output> = rx.try_iter().collect();
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Output::Partial(_, p) if p.contains("Proceed?"))),
            "the prompt must arrive before any newline does: {msgs:?}"
        );
        assert!(
            result.lines.iter().any(|l| l.contains("got:yes")),
            "the answer must reach the command's stdin: {:?}",
            result.lines
        );
    }

    /// Two commands in sequence must both be answerable.
    ///
    /// This is the case that broke a real upgrade: `pacman -Syu` succeeded
    /// interactively, then `paru -Sua` ran with `--noconfirm` because the
    /// answers channel had already been consumed, and paru refused the conflict
    /// outright rather than asking.
    #[test]
    fn a_second_command_can_still_be_answered() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let (itx, irx) = std::sync::mpsc::channel();
        let slot = stdin_slot();
        spawn_answer_writer(irx, slot.clone());

        std::thread::spawn(move || {
            for answer in ["first", "second"] {
                std::thread::sleep(std::time::Duration::from_millis(120));
                let _ = itx.send(answer.to_string());
            }
        });

        let script = "read a; echo \"got:$a\"".to_string();
        let one = run_inner("sh", &["-c".to_string(), script.clone()], false, Some(&slot), &tx);
        let two = run_inner("sh", &["-c".to_string(), script], false, Some(&slot), &tx);

        assert!(one.lines.iter().any(|l| l.contains("got:first")), "{:?}", one.lines);
        assert!(
            two.lines.iter().any(|l| l.contains("got:second")),
            "the second command must reach the same answers channel: {:?}",
            two.lines
        );
    }

    /// A prompt on stdout must survive traffic on stderr.
    ///
    /// This is the bug that stalled a real upgrade: pacman asked
    /// ":: Proceed with installation? [Y/n]" on stdout and wrote status to
    /// stderr, one shared fragment slot meant the question was erased, and the
    /// dialog sat for minutes waiting on something the user could not see.
    #[test]
    fn a_stdout_prompt_is_not_erased_by_stderr_output() {
        let (tx, rx) = std::sync::mpsc::channel();
        let slot = stdin_slot();
        let (itx, irx) = std::sync::mpsc::channel();
        spawn_answer_writer(irx, slot.clone());
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = itx.send("y".to_string());
        });

        // Prompt on stdout, then chatter on stderr, then wait for an answer —
        // exactly pacman's shape.
        let result = run_inner(
            "sh",
            &[
                "-c".to_string(),
                "printf ':: Proceed? [Y/n] '; echo 'status line' >&2; read a; echo \"got:$a\""
                    .to_string(),
            ],
            false,
            Some(&slot),
            &tx,
        );
        assert!(result.success);

        let msgs: Vec<Output> = rx.try_iter().collect();
        let prompt = msgs.iter().rev().find_map(|m| match m {
            Output::Partial(Stream::Stdout, p) if p.contains("Proceed?") => Some(p.clone()),
            _ => None,
        });
        assert!(prompt.is_some(), "the stdout prompt must be reported: {msgs:?}");
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Output::Line(Stream::Stderr, l) if l.contains("status line"))),
            "and stderr must still be reported, tagged as stderr: {msgs:?}"
        );
    }

    /// Interrupting must actually stop a long-running command.
    ///
    /// The alternative is a dialog that takes every key while a build runs for
    /// twenty minutes, whose only exit is killing the program — which orphans a
    /// live pacman transaction still holding the database lock.
    #[test]
    fn a_running_command_can_be_interrupted() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let pids = pid_slot();
        let watch = pids.clone();

        std::thread::spawn(move || {
            // Wait for the pid to be published, then interrupt.
            for _ in 0..100 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                if watch.lock().map(|g| g.is_some()).unwrap_or(false) {
                    assert!(interrupt(&watch), "kill should succeed on a live child");
                    return;
                }
            }
            panic!("the pid was never published");
        });

        let started = std::time::Instant::now();
        let result = run_tracked(
            "sh",
            &["-c".to_string(), "sleep 30".to_string()],
            false,
            None,
            Some(&pids),
            &tx,
        );

        assert!(!result.success, "an interrupted command must not report success");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "it should stop promptly, not run to completion"
        );
        // And the slot is cleared, so a later interrupt cannot hit a reused pid.
        assert!(pids.lock().unwrap().is_none());
    }

    #[test]
    fn complete_lines_still_arrive_as_lines() {
        let (tx, rx) = std::sync::mpsc::channel();
        let result = run_inner(
            "sh",
            &["-c".to_string(), "echo one; echo two".to_string()],
            false,
            None,
            &tx,
        );
        assert!(result.success);
        let lines: Vec<String> = rx
            .try_iter()
            .filter_map(|m| match m {
                Output::Line(_, l) => Some(l),
                _ => None,
            })
            .collect();
        assert!(lines.contains(&"one".to_string()), "{lines:?}");
        assert!(lines.contains(&"two".to_string()), "{lines:?}");
    }
}
