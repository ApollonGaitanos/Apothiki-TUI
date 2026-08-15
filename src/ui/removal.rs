//! The removal dialog and its state machine.
//!
//! The sequence is deliberately rigid, because every step exists to stop a
//! different way of destroying a system:
//!
//! ```text
//! Choose mode  →  denylist check  →  impact preview  →  confirm
//!              →  pacman --print dry-run  →  reconcile against our simulation
//!              →  sudo pre-auth  →  snapper snapshot  →  execute  →  log
//! ```
//!
//! The reconcile step is the one that cannot be skipped. Our in-process
//! simulation matches `pacman -Rs --print` on all 1656 packages of the dev
//! machine, but a plan is checked against pacman again immediately before it
//! runs. Removing the wrong package is worse than refusing to remove anything.

use std::sync::mpsc::{Receiver, Sender};

use crate::data::graph::PkgIdx;
use crate::ops::exec::{self, AuthState, Output, Secret};
use crate::ops::safety::Risk;
use crate::ops::{history, snapshot, RemovalMode, RemovalRequest};

/// Which step of the flow is on screen.
pub enum Stage {
    /// Choosing a removal mode and reading the impact.
    Confirm,
    /// Risk tier is Dangerous: the package name must be typed out in full.
    TypeToConfirm,
    /// A password is needed to refresh the sudo timestamp.
    Password,
    /// The command is running; output streams in.
    Running,
    /// Finished, successfully or not.
    Done { success: bool },
}

pub struct RemovalDialog {
    pub request: RemovalRequest,
    pub stage: Stage,
    pub mode_index: usize,
    /// What the user has typed for confirmation, or into the password field.
    pub typed: String,
    pub password: String,
    /// Whether to take a snapper snapshot first. On by default when available:
    /// it converts a mistake into a reboot (spec §6.4).
    pub snapshot: bool,
    pub error: Option<String>,
    pub output: Vec<String>,
    pub receiver: Option<Receiver<Output>>,
    /// The exact name the user must type, cached so it cannot drift.
    pub confirm_word: String,
}

impl RemovalDialog {
    pub fn new(request: RemovalRequest, confirm_word: String) -> Self {
        let mode_index = RemovalMode::ALL
            .iter()
            .position(|m| *m == request.mode)
            .unwrap_or(0);
        RemovalDialog {
            snapshot: request.snapshot,
            request,
            stage: Stage::Confirm,
            mode_index,
            typed: String::new(),
            password: String::new(),
            error: None,
            output: Vec::new(),
            receiver: None,
            confirm_word,
        }
    }

    pub fn mode(&self) -> RemovalMode {
        RemovalMode::ALL[self.mode_index]
    }

    /// True when the typed confirmation matches. Compared exactly — a
    /// case-insensitive or trimmed match would defeat the point of asking.
    pub fn confirmation_satisfied(&self) -> bool {
        self.typed == self.confirm_word
    }

    /// Whether the dialog can proceed from the confirm stage.
    pub fn can_proceed(&self) -> bool {
        if self.request.is_blocked() {
            return false;
        }
        match self.stage {
            Stage::Confirm => !self.request.risk.needs_typed_confirmation(),
            Stage::TypeToConfirm => self.confirmation_satisfied(),
            _ => false,
        }
    }
}

/// The outcome of asking the dialog to advance.
pub enum Advance {
    /// Stay open, nothing else to do.
    Stay,
    /// Move to typed confirmation.
    NeedsTyping,
    /// Ready to run; the caller should start the operation.
    Execute,
    /// Needs a password first.
    NeedsPassword,
    /// Abort with a message.
    Abort(String),
}

/// Validates a plan against pacman immediately before running it.
///
/// Returns an error describing any divergence. This is the last gate, and a
/// divergence is always fatal to the operation: it means our model of the
/// system is wrong, and acting on a wrong model is how a package manager eats
/// somebody's desktop.
pub fn verify_against_pacman(
    request: &RemovalRequest,
    graph: &crate::data::graph::Graph,
) -> Result<Vec<String>, String> {
    let names = request.package_names(graph);
    let args = request.mode.dry_run_args(&names);

    let theirs = exec::dry_run(&args).map_err(|e| format!("pacman refused this removal: {e}"))?;

    let ours: Vec<String> = if request.mode.takes_dependencies() {
        request
            .plan
            .all_removed()
            .iter()
            .map(|&p| graph.name(p).to_string())
            .collect()
    } else {
        names.clone()
    };

    let known: Vec<String> = graph
        .db
        .packages
        .iter()
        .map(|p| p.name.clone())
        .collect();

    let diff = exec::reconcile(&ours, &theirs, &known);
    if !diff.is_empty() {
        return Err(format!(
            "aborted: our preview disagrees with pacman.\n{}",
            diff.join("\n")
        ));
    }
    Ok(theirs)
}

/// Spawns the privileged work on a background thread.
///
/// Snapshot first, then the removal: a snapshot taken after the fact protects
/// nothing.
pub fn spawn(
    packages: Vec<String>,
    versions: Vec<(String, String)>,
    mode: RemovalMode,
    take_snapshot: bool,
    tx: Sender<Output>,
) {
    std::thread::spawn(move || {
        let mut snapshot_id: Option<String> = None;

        if take_snapshot {
            if let Some(config) = snapshot::config_name() {
                let desc = format!("apothiki: {} {}", mode.flags(), packages.join(" "));
                let (stx, srx) = std::sync::mpsc::channel();
                let args = snapshot::pre_snapshot_args(&config, &desc);
                exec::run_privileged("snapper", &args, stx);

                let mut ok = false;
                for msg in srx {
                    match msg {
                        Output::Line(l) => {
                            let _ = tx.send(Output::Line(format!("snapshot: {l}")));
                            if l.trim().chars().all(|c| c.is_ascii_digit()) && !l.trim().is_empty()
                            {
                                snapshot_id = Some(l.trim().to_string());
                            }
                        }
                        Output::Finished { success, .. } => ok = success,
                        Output::Failed(e) => {
                            let _ = tx.send(Output::Line(format!("snapshot failed: {e}")));
                        }
                    }
                }

                if !ok {
                    // The snapshot is the safety net the user opted into. If it
                    // could not be taken, do not proceed as though it had been.
                    let _ = tx.send(Output::Failed(
                        "snapshot failed — removal aborted (uncheck the snapshot option to \
                         proceed without one)"
                            .into(),
                    ));
                    return;
                }
            }
        }

        let (rtx, rrx) = std::sync::mpsc::channel();
        exec::run_privileged("pacman", &mode.args(&packages), rtx);

        let mut success = false;
        for msg in rrx {
            match msg {
                Output::Line(l) => {
                    let _ = tx.send(Output::Line(l));
                }
                Output::Finished { success: s, code } => {
                    success = s;
                    let _ = tx.send(Output::Finished { success: s, code });
                }
                Output::Failed(e) => {
                    let _ = tx.send(Output::Failed(e));
                }
            }
        }

        let entry = history::Entry {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            operation: mode.flags().to_string(),
            packages: versions,
            success,
            snapshot: snapshot_id,
        };
        if let Err(e) = history::record(&entry) {
            let _ = tx.send(Output::Line(format!("(could not write history: {e})")));
        }
    });
}

/// Checks whether authentication is needed before running.
pub fn auth_stage() -> AuthState {
    exec::check_auth()
}

/// Validates a typed password, consuming it.
pub fn try_authenticate(password: String) -> bool {
    let secret = Secret::new(password);
    if secret.is_empty() {
        return false;
    }
    exec::authenticate(&secret).unwrap_or(false)
}

/// Human wording for a risk tier, shown above the confirmation.
pub fn risk_sentence(risk: Risk, apps_lost: &[String]) -> String {
    match risk {
        Risk::Blocked => "This is part of the system and cannot be removed.".into(),
        Risk::Dangerous if !apps_lost.is_empty() => {
            format!(
                "This removes {}. Type the name below to confirm.",
                apps_lost.join(", ")
            )
        }
        Risk::Dangerous => "This is a large or far-reaching removal. Type the name to confirm.".into(),
        Risk::Caution => "Other packages are involved. Check the list before continuing.".into(),
        Risk::Safe => "Nothing else depends on this.".into(),
    }
}

/// Targets for a bulk orphan cleanup.
///
/// Restricted to the orphans pacman itself reports. Our reachability pass finds
/// more — packages stranded under an orphan root, which refcounting cannot see —
/// and those findings are sound, but "sound analysis" is a poor basis for a
/// bulk delete of packages no pacman command would have offered. They stay
/// individually removable instead.
pub fn bulk_orphan_targets(
    graph: &crate::data::graph::Graph,
    pacman_reported: &[String],
) -> Vec<PkgIdx> {
    pacman_reported
        .iter()
        .filter_map(|n| graph.index_of(n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_confirmation_must_match_exactly() {
        let word = "godot".to_string();
        let mut d = dialog(word.clone());
        d.stage = Stage::TypeToConfirm;

        for wrong in ["", "God", "godo", "GODOT", " godot", "godot "] {
            d.typed = wrong.into();
            assert!(!d.confirmation_satisfied(), "{wrong:?} must not pass");
        }
        d.typed = "godot".into();
        assert!(d.confirmation_satisfied());
    }

    #[test]
    fn risk_sentence_names_the_applications_at_stake() {
        let s = risk_sentence(Risk::Dangerous, &["GIMP".into(), "Inkscape".into()]);
        assert!(s.contains("GIMP") && s.contains("Inkscape"), "{s}");
    }

    #[test]
    fn bulk_cleanup_only_takes_what_pacman_reported() {
        // Guards the decision that a bulk delete never includes our own extra
        // findings, however confident we are in them.
        use crate::data::local::{LocalDb, Reason};
        use std::sync::Arc;

        let mut packages: Vec<_> = ["npm", "nodejs", "libcbor"]
            .iter()
            .map(|n| {
                crate::data::local::parse_desc(
                    &format!("%NAME%\n{n}\n\n%VERSION%\n1-1\n\n%REASON%\n1\n"),
                    n,
                )
                .unwrap()
            })
            .collect();
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        let _ = Reason::Dependency;

        let g = crate::data::graph::Graph::build(Arc::new(LocalDb {
            packages,
            errors: vec![],
            root: Default::default(),
        }));

        let reported = vec!["libcbor".to_string()];
        let targets = bulk_orphan_targets(&g, &reported);
        assert_eq!(targets.len(), 1);
        assert_eq!(g.name(targets[0]), "libcbor");
    }

    fn dialog(word: String) -> RemovalDialog {
        use crate::data::graph::RemovalPlan;
        RemovalDialog::new(
            RemovalRequest {
                targets: vec![],
                mode: RemovalMode::WithUnusedDeps,
                plan: RemovalPlan::default(),
                risk: Risk::Dangerous,
                blocked_by: None,
                apps_lost: vec![],
                snapshot: false,
            },
            word,
        )
    }
}
