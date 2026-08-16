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

/// How far the typed confirmation has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmState {
    Empty,
    /// A correct prefix so far — nothing is wrong yet.
    Incomplete,
    /// Diverges from the required name.
    Wrong,
    Matches,
}

impl ConfirmState {
    pub fn hint(&self, word: &str) -> String {
        match self {
            ConfirmState::Empty => format!("type \"{word}\" exactly"),
            ConfirmState::Incomplete => "keep going…".into(),
            ConfirmState::Wrong => format!("that does not match \"{word}\""),
            ConfirmState::Matches => "matches — press Enter to remove".into(),
        }
    }
}

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

/// What the dialog is about to do.
///
/// Restore shares the dialog because it shares everything that matters: the
/// confirm/authenticate/stream/report machine. Only the summary text and the
/// command differ, and duplicating the state machine is exactly how the two
/// would drift apart.
pub enum Job {
    Remove(RemovalRequest),
    Restore(crate::ops::restore::RestorePlan),
    Install(crate::ops::InstallRequest),
    Update(crate::ops::update::UpdatePlan),
}

impl Job {
    pub fn is_restore(&self) -> bool {
        matches!(self, Job::Restore(_))
    }

    pub fn as_removal(&self) -> Option<&RemovalRequest> {
        match self {
            Job::Remove(r) => Some(r),
            _ => None,
        }
    }

    pub fn as_restore(&self) -> Option<&crate::ops::restore::RestorePlan> {
        match self {
            Job::Restore(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_install(&self) -> Option<&crate::ops::InstallRequest> {
        match self {
            Job::Install(r) => Some(r),
            _ => None,
        }
    }

    pub fn as_update(&self) -> Option<&crate::ops::update::UpdatePlan> {
        match self {
            Job::Update(p) => Some(p),
            _ => None,
        }
    }
}

pub struct RemovalDialog {
    pub job: Job,
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
    /// A fetched PKGBUILD, shown before an AUR install.
    pub pkgbuild: Option<String>,
    pub pkgbuild_scroll: u16,
    pkgbuild_rx: Option<Receiver<anyhow::Result<String>>>,
}

impl RemovalDialog {
    pub fn new(request: RemovalRequest, confirm_word: String) -> Self {
        let mode_index = RemovalMode::ALL
            .iter()
            .position(|m| *m == request.mode)
            .unwrap_or(0);
        RemovalDialog {
            snapshot: request.snapshot,
            job: Job::Remove(request),
            stage: Stage::Confirm,
            mode_index,
            typed: String::new(),
            password: String::new(),
            error: None,
            output: Vec::new(),
            receiver: None,
            confirm_word,
            pkgbuild: None,
            pkgbuild_scroll: 0,
            pkgbuild_rx: None,
        }
    }

    /// Restoring a removed package is not destructive, so it takes a snapshot
    /// only if one is available and never demands a typed confirmation.
    pub fn restore(plan: crate::ops::restore::RestorePlan) -> Self {
        RemovalDialog {
            snapshot: false,
            job: Job::Restore(plan),
            stage: Stage::Confirm,
            mode_index: 0,
            typed: String::new(),
            password: String::new(),
            error: None,
            output: Vec::new(),
            receiver: None,
            confirm_word: String::new(),
            pkgbuild: None,
            pkgbuild_scroll: 0,
            pkgbuild_rx: None,
        }
    }

    /// The removal request, for the paths that only apply to removals.
    pub fn request(&self) -> Option<&RemovalRequest> {
        self.job.as_removal()
    }

    pub fn mode(&self) -> RemovalMode {
        RemovalMode::ALL[self.mode_index]
    }

    /// Whether this job may proceed at all.
    pub fn blocked(&self) -> bool {
        match &self.job {
            Job::Remove(r) => r.is_blocked(),
            // A restore that cannot be completed in full is never offered.
            Job::Restore(p) => !p.is_complete(),
            Job::Install(r) => {
                r.source == crate::ops::InstallSource::Aur && r.helper.is_none()
            }
            Job::Update(p) => p.is_empty(),
        }
    }

    pub fn needs_typed_confirmation(&self) -> bool {
        match &self.job {
            Job::Remove(r) => r.risk.needs_typed_confirmation(),
            // Installing adds something; it does not destroy anything, so the
            // strongest confirmation is reserved for removal.
            _ => false,
        }
    }

    /// Builds an update dialog. A snapshot is on by default: a system upgrade
    /// touches more of the machine than any single removal.
    pub fn update(plan: crate::ops::update::UpdatePlan) -> Self {
        RemovalDialog {
            snapshot: crate::ops::snapshot::is_available(),
            job: Job::Update(plan),
            stage: Stage::Confirm,
            mode_index: 0,
            typed: String::new(),
            password: String::new(),
            error: None,
            output: Vec::new(),
            receiver: None,
            confirm_word: String::new(),
            pkgbuild: None,
            pkgbuild_scroll: 0,
            pkgbuild_rx: None,
        }
    }

    /// Builds an install dialog.
    pub fn install(request: crate::ops::InstallRequest) -> Self {
        RemovalDialog {
            snapshot: false,
            job: Job::Install(request),
            stage: Stage::Confirm,
            mode_index: 0,
            typed: String::new(),
            password: String::new(),
            error: None,
            output: Vec::new(),
            receiver: None,
            confirm_word: String::new(),
            pkgbuild: None,
            pkgbuild_scroll: 0,
            pkgbuild_rx: None,
        }
    }

    /// Starts fetching the PKGBUILD for an AUR install.
    pub fn request_pkgbuild(&mut self) {
        if self.pkgbuild.is_some() || self.pkgbuild_rx.is_some() {
            return;
        }
        let Some(request) = self.job.as_install() else {
            return;
        };
        if request.source != crate::ops::InstallSource::Aur {
            return;
        }
        let package = request.package.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::data::aur::fetch_pkgbuild(&package));
        });
        self.pkgbuild_rx = Some(rx);
        self.pkgbuild = Some("fetching PKGBUILD…".to_string());
    }

    /// Collects a fetched PKGBUILD, if one has arrived.
    pub fn pump_pkgbuild(&mut self) {
        let Some(rx) = &self.pkgbuild_rx else { return };
        match rx.try_recv() {
            Ok(Ok(text)) => {
                self.pkgbuild = Some(text);
                self.pkgbuild_rx = None;
            }
            Ok(Err(e)) => {
                self.pkgbuild = Some(format!("could not fetch PKGBUILD: {e}"));
                self.pkgbuild_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.pkgbuild_rx = None,
        }
    }

    /// True when the typed confirmation matches. Compared exactly — a
    /// case-insensitive or trimmed match would defeat the point of asking.
    pub fn confirmation_satisfied(&self) -> bool {
        self.typed == self.confirm_word
    }

    /// How the typed text relates to the required word, for live feedback.
    ///
    /// Without this the only signal for a mistyped name is Enter doing
    /// nothing, which reads as the tool being broken rather than as the user
    /// having made a typo.
    pub fn confirmation_state(&self) -> ConfirmState {
        if self.typed.is_empty() {
            ConfirmState::Empty
        } else if self.typed == self.confirm_word {
            ConfirmState::Matches
        } else if self.confirm_word.starts_with(&self.typed) {
            ConfirmState::Incomplete
        } else {
            ConfirmState::Wrong
        }
    }

    /// Whether the dialog can proceed from the confirm stage.
    pub fn can_proceed(&self) -> bool {
        if self.blocked() {
            return false;
        }
        match self.stage {
            Stage::Confirm => !self.needs_typed_confirmation(),
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

/// Spawns a full system upgrade, then AUR upgrades if any.
///
/// Repository packages go first and as one transaction. Upgrading a subset is a
/// partial upgrade, which on a rolling release leaves binaries linked against
/// libraries that are no longer installed — so the plan is never narrowed, no
/// matter which package the user was looking at when they pressed the key.
pub fn spawn_update(
    plan: crate::ops::update::UpdatePlan,
    helper: Option<String>,
    take_snapshot: bool,
    tx: Sender<Output>,
) {
    use crate::ops::update::UpdatePlan;

    std::thread::spawn(move || {
        if dry_run_mode() {
            let _ = tx.send(Output::Line("APOTHIKI_DRY_RUN is set — nothing will change".into()));
            if take_snapshot {
                let _ = tx.send(Output::Line("would take a snapper snapshot".into()));
            }
            let _ = tx.send(Output::Line(format!(
                "would run: sudo pacman {}",
                UpdatePlan::system_upgrade_args().join(" ")
            )));
            if !plan.aur.is_empty() {
                let _ = tx.send(Output::Line(format!(
                    "would then run: {} {}",
                    helper.clone().unwrap_or_else(|| "paru".into()),
                    UpdatePlan::aur_upgrade_args().join(" ")
                )));
            }
            let _ = tx.send(Output::Finished { success: true, code: Some(0) });
            return;
        }

        if take_snapshot {
            if let Some(config) = snapshot::config_name() {
                let args = snapshot::pre_snapshot_args(&config, "apothiki: system upgrade");
                let _ = tx.send(Output::Line("taking snapshot…".into()));
                if !exec::run_privileged("snapper", &args, &tx).success {
                    let _ = tx.send(Output::Failed(
                        "snapshot failed — upgrade aborted (uncheck the snapshot option to \
                         proceed without one)"
                            .into(),
                    ));
                    return;
                }
            }
        }

        let repo = exec::run_privileged("pacman", &UpdatePlan::system_upgrade_args(), &tx);
        let mut success = repo.success;

        // AUR rebuilds only after the repository upgrade succeeded: building
        // against a half-upgraded system is exactly the failure the ordering
        // exists to avoid.
        if success && !plan.aur.is_empty() {
            if let Some(h) = &helper {
                let aur = exec::run_unprivileged(h, &UpdatePlan::aur_upgrade_args(), &tx);
                success = aur.success;
            } else {
                let _ = tx.send(Output::Line(
                    "no AUR helper found; AUR packages were not upgraded".into(),
                ));
            }
        }

        let _ = tx.send(Output::Finished { success, code: repo.code });

        let entry = history::Entry {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            operation: "-Syu".to_string(),
            packages: plan
                .repo
                .iter()
                .chain(plan.aur.iter())
                .map(|u| (u.name.clone(), u.available.clone()))
                .collect(),
            success,
            snapshot: None,
        };
        let _ = history::record(&entry);
    });
}

/// Spawns an install on a background thread.
///
/// Repository packages go through the same privileged path as everything else.
/// AUR packages are handed to a helper, which is run **as the user, not under
/// sudo** — helpers refuse to run as root, and correctly so: makepkg builds
/// untrusted source, and building it as root is how a malicious PKGBUILD owns
/// the machine. The helper escalates on its own for the final install step,
/// which succeeds without prompting because we pre-authenticated.
pub fn spawn_install(request: crate::ops::InstallRequest, tx: Sender<Output>) {
    std::thread::spawn(move || {
        if dry_run_mode() {
            let _ = tx.send(Output::Line(
                "APOTHIKI_DRY_RUN is set — nothing will be installed".into(),
            ));
            let _ = tx.send(Output::Line(format!("would run: {}", request.command_line())));
            let _ = tx.send(Output::Finished { success: true, code: Some(0) });
            return;
        }

        let result = match request.source {
            crate::ops::InstallSource::Repo => {
                exec::run_privileged("pacman", &request.args(), &tx)
            }
            crate::ops::InstallSource::Aur => {
                let helper = request.helper.clone().unwrap_or_else(|| "paru".into());
                exec::run_unprivileged(&helper, &request.args(), &tx)
            }
        };

        let _ = tx.send(Output::Finished {
            success: result.success,
            code: result.code,
        });

        let entry = history::Entry {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            operation: "-S".to_string(),
            packages: vec![(request.package.clone(), request.version.clone())],
            success: result.success,
            snapshot: None,
        };
        let _ = history::record(&entry);
    });
}

/// Spawns a restore on a background thread.
///
/// Deliberately simpler than the removal path: no snapshot, because putting
/// packages back is not the operation that needs undoing, and every file comes
/// from the local cache so nothing is fetched.
pub fn spawn_restore(plan: crate::ops::restore::RestorePlan, tx: Sender<Output>) {
    std::thread::spawn(move || {
        if dry_run_mode() {
            let _ = tx.send(Output::Line(
                "APOTHIKI_DRY_RUN is set — nothing will be installed".into(),
            ));
            let _ = tx.send(Output::Line(format!("would run: {}", plan.command_line())));
            let _ = tx.send(Output::Finished { success: true, code: Some(0) });
            return;
        }

        let result = exec::run_privileged("pacman", &plan.args(), &tx);
        let success = result.success;
        let _ = tx.send(Output::Finished {
            success,
            code: result.code,
        });

        let entry = history::Entry {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            operation: "-U (undo)".to_string(),
            packages: plan
                .available
                .iter()
                .map(|(n, v, _)| (n.clone(), v.clone()))
                .collect(),
            success,
            snapshot: None,
        };
        let _ = history::record(&entry);
    });
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
        // Development escape hatch: exercise the whole pipeline — confirmation,
        // dry-run, reconcile, streaming, history — without mutating anything.
        // Exists so the execution path can be tested honestly rather than by
        // reading it and hoping.
        if dry_run_mode() {
            let _ = tx.send(Output::Line("APOTHIKI_DRY_RUN is set — nothing will be removed".into()));
            if take_snapshot {
                let _ = tx.send(Output::Line("would take a snapper pre-transaction snapshot".into()));
            }
            let _ = tx.send(Output::Line(format!(
                "would run: sudo pacman {}",
                mode.args(&packages).join(" ")
            )));
            match exec::dry_run(&mode.dry_run_args(&packages)) {
                Ok(lines) => {
                    for l in lines {
                        let _ = tx.send(Output::Line(format!("  would remove {l}")));
                        // Paced so that live streaming is observable in this
                        // mode; the real path streams at pacman's own rate.
                        std::thread::sleep(std::time::Duration::from_millis(120));
                    }
                    let _ = tx.send(Output::Finished { success: true, code: Some(0) });
                }
                Err(e) => {
                    let _ = tx.send(Output::Failed(e.to_string()));
                }
            }
            return;
        }

        let mut snapshot_id: Option<String> = None;

        if take_snapshot {
            if let Some(config) = snapshot::config_name() {
                let desc = format!("apothiki: {} {}", mode.flags(), packages.join(" "));
                let args = snapshot::pre_snapshot_args(&config, &desc);
                let _ = tx.send(Output::Line("taking snapshot…".into()));
                let result = exec::run_privileged("snapper", &args, &tx);

                // `--print-number` writes the new snapshot's id on its own line.
                snapshot_id = result
                    .lines
                    .iter()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()))
                    .map(|l| l.to_string());

                if !result.success {
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

        let result = exec::run_privileged("pacman", &mode.args(&packages), &tx);
        let success = result.success;
        let _ = tx.send(Output::Finished {
            success,
            code: result.code,
        });

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

/// Whether the development dry-run mode is active.
pub fn dry_run_mode() -> bool {
    std::env::var_os("APOTHIKI_DRY_RUN").is_some()
}

/// Checks whether authentication is needed before running.
///
/// Dry-run needs no privileges, so it never prompts.
pub fn auth_stage() -> AuthState {
    if dry_run_mode() {
        return AuthState::Ready;
    }
    exec::check_auth()
}

/// Validates a typed password, consuming it. Returns sudo's own message on
/// failure rather than a generic one.
pub fn try_authenticate(password: String) -> Result<(), String> {
    let secret = Secret::new(password);
    if secret.is_empty() {
        return Err("no password entered".into());
    }
    exec::authenticate(&secret)
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
    fn confirmation_state_distinguishes_incomplete_from_wrong() {
        // A half-typed name is not an error; a wrong one is. Reporting both the
        // same way trains the user to ignore the message.
        let mut d = dialog("alacritty".into());
        d.stage = Stage::TypeToConfirm;

        assert_eq!(d.confirmation_state(), ConfirmState::Empty);
        d.typed = "ala".into();
        assert_eq!(d.confirmation_state(), ConfirmState::Incomplete);
        d.typed = "alax".into();
        assert_eq!(d.confirmation_state(), ConfirmState::Wrong);
        d.typed = "Alacritty".into();
        assert_eq!(d.confirmation_state(), ConfirmState::Wrong);
        d.typed = "alacritty".into();
        assert_eq!(d.confirmation_state(), ConfirmState::Matches);
        d.typed = "alacritty ".into();
        assert_eq!(d.confirmation_state(), ConfirmState::Wrong);
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
