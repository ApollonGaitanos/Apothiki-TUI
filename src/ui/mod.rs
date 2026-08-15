//! The TUI: state, event handling, and the four M1 views.
//!
//! Bindings are CUA (spec §8, decided): arrows and Tab to move, F-keys to switch
//! view, Ctrl+F to search, F1 for help, Esc to back out, Ctrl+Q to quit. No
//! modal `hjkl` navigation — the audience is a user who does not want to learn
//! a text editor to see what is installed.
//!
//! The key hint bar is always visible. Discoverability *is* the noob protection
//! the spec asks for; a hidden binding may as well not exist.

pub mod removal;
pub mod render;
pub mod term;

use std::collections::HashMap;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use crate::apps::Source;
use crate::data::graph::{OrphanMode, PkgIdx, RemovalPlan};
use crate::data::local::Reason;
use crate::ops::safety::Denylist;
use crate::ops::{RemovalMode, RemovalRequest};
use crate::state::SystemState;
use removal::{RemovalDialog, Stage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum View {
    Apps,
    Tools,
    Dependencies,
    Orphans,
}

impl View {
    pub const ALL: [View; 4] = [View::Apps, View::Tools, View::Dependencies, View::Orphans];

    /// Titles carry their key, because the hint bar is the discoverability
    /// mechanism. Views moved to the number row so plain typing stays free for
    /// other uses and the F-keys keep their conventional meanings (F1 help,
    /// F5 refresh); F2-F6 remain as undocumented aliases.
    pub fn title(&self) -> &'static str {
        match self {
            View::Apps => "1 Apps",
            View::Tools => "2 Tools",
            View::Dependencies => "3 Dependencies",
            View::Orphans => "4 Orphans",
        }
    }
}

/// What a list row points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// Index into `catalog.apps`.
    App(usize),
    /// Index into `catalog.tools`.
    Tool(usize),
    Package(PkgIdx),
}

/// Which pane has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    /// The related-packages list in the detail pane, used to walk the graph.
    Related,
}

/// A row in the relationships pane.
///
/// The removal action sits at index 0, above the dependencies, so that pressing
/// Up from the first relationship lands on it.
#[derive(Debug, Clone)]
pub enum RelatedRow {
    /// "Remove this…" — opens the removal dialog.
    RemoveAction,
    Relation(Related),
}

/// One entry in the detail pane's related-packages list.
#[derive(Debug, Clone)]
pub struct Related {
    pub pkg: PkgIdx,
    pub kind: RelationKind,
    /// The `optdepends` reason string, which explains what is lost if removed.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    DependsOn,
    RequiredBy,
    /// Not a graph edge. Removing one degrades the dependent silently, which is
    /// why it is shown separately rather than mixed in with real dependencies.
    Optional,
}

impl RelationKind {
    pub fn label(&self) -> &'static str {
        match self {
            RelationKind::DependsOn => "depends on",
            RelationKind::RequiredBy => "required by",
            RelationKind::Optional => "optional",
        }
    }
}

pub struct Ui {
    pub state: SystemState,
    pub view: View,
    pub focus: Focus,
    /// Selected row per view, so switching tabs does not lose your place.
    pub selected: HashMap<View, usize>,
    pub scroll: usize,
    pub related_selected: usize,
    pub query: String,
    pub searching: bool,
    pub show_help: bool,
    pub should_quit: bool,
    /// Where we came from, for jumping back out of a graph walk.
    ///
    /// The search query is part of the position, not incidental to it: a row
    /// index only means something relative to the filter that produced it, so
    /// restoring the index without the query lands somewhere else entirely.
    pub history: Vec<(View, usize, String)>,
    /// Cached removal simulation for the current selection. Recomputed only when
    /// the selection changes, never during a render.
    impact: Option<(PkgIdx, RemovalPlan)>,
    /// package name → indices of apps it backs, for naming the applications a
    /// removal would take with it.
    apps_by_package: HashMap<String, Vec<usize>>,
    pub orphan_mode: OrphanMode,
    /// Rows for the current view and query, rebuilt when either changes.
    rows: Vec<Item>,
    pub enhanced_keys: bool,
    /// Packages that may never be removed, with the reason (spec §6.1).
    pub denylist: Denylist,
    /// The removal dialog, when open. While it is open it takes every key.
    pub dialog: Option<RemovalDialog>,
    /// Package names backing a visible application, for risk assessment.
    app_package_names: std::collections::HashSet<String>,
    /// package name → the applications it backs, for naming what would be lost.
    apps_named_by_package: HashMap<String, Vec<String>>,
    /// Set after a removal completes, so the next tick can reload.
    pub needs_reload: bool,
}

impl Ui {
    pub fn new(state: SystemState, enhanced_keys: bool) -> Self {
        let mut apps_by_package: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, app) in state.catalog.apps.iter().enumerate() {
            for p in &app.packages {
                apps_by_package.entry(p.clone()).or_default().push(i);
            }
        }

        let denylist = Denylist::build(&state.graph);
        let app_package_names: std::collections::HashSet<String> = state
            .catalog
            .apps
            .iter()
            .flat_map(|a| a.packages.iter().cloned())
            .collect();
        let mut apps_named_by_package: HashMap<String, Vec<String>> = HashMap::new();
        for app in &state.catalog.apps {
            for p in &app.packages {
                apps_named_by_package
                    .entry(p.clone())
                    .or_default()
                    .push(app.name.clone());
            }
        }

        let mut ui = Ui {
            state,
            view: View::Apps,
            focus: Focus::List,
            selected: HashMap::new(),
            scroll: 0,
            related_selected: 0,
            query: String::new(),
            searching: false,
            show_help: false,
            should_quit: false,
            history: Vec::new(),
            impact: None,
            apps_by_package,
            orphan_mode: OrphanMode::Conservative,
            rows: Vec::new(),
            enhanced_keys,
            denylist,
            dialog: None,
            app_package_names,
            apps_named_by_package,
            needs_reload: false,
        };
        ui.rebuild_rows();
        ui
    }

    pub fn rows(&self) -> &[Item] {
        &self.rows
    }

    pub fn selection(&self) -> usize {
        *self.selected.get(&self.view).unwrap_or(&0)
    }

    fn set_selection(&mut self, i: usize) {
        self.selected.insert(self.view, i);
        self.impact = None;
        self.related_selected = 0;
    }

    pub fn current(&self) -> Option<Item> {
        self.rows.get(self.selection()).copied()
    }

    /// Rebuilds the visible row list for the current view and query.
    ///
    /// Plain case-insensitive substring matching. Fuzzy ranking with `nucleo`
    /// arrives with the search view in M3; pretending to rank here would be
    /// worse than an honest filter.
    pub fn rebuild_rows(&mut self) {
        let q = self.query.to_lowercase();
        let matches = |s: &str| q.is_empty() || s.to_lowercase().contains(&q);

        self.rows = match self.view {
            View::Apps => self
                .state
                .catalog
                .apps
                .iter()
                .enumerate()
                .filter(|(_, a)| matches(&a.name) || a.packages.iter().any(|p| matches(p)))
                .map(|(i, _)| Item::App(i))
                .collect(),
            View::Tools => self
                .state
                .catalog
                .tools
                .iter()
                .enumerate()
                .filter(|(_, a)| matches(&a.name))
                .map(|(i, _)| Item::Tool(i))
                .collect(),
            View::Dependencies => self
                .state
                .db
                .packages
                .iter()
                .enumerate()
                .filter(|(_, p)| p.reason == Reason::Dependency && matches(&p.name))
                .map(|(i, _)| Item::Package(i as u32))
                .collect(),
            View::Orphans => {
                let mut v: Vec<Item> = self
                    .state
                    .graph
                    .orphans(self.orphan_mode)
                    .into_iter()
                    .filter(|&i| matches(&self.state.db.packages[i as usize].name))
                    .map(Item::Package)
                    .collect();
                v.sort_by_key(|i| match i {
                    Item::Package(p) => *p,
                    _ => 0,
                });
                v
            }
        };

        let max = self.rows.len().saturating_sub(1);
        if self.selection() > max {
            self.set_selection(max);
        }
    }

    /// The package a row refers to, if any. Apps backed by Flatpak, AppImage or
    /// Steam have none, which is a fact the detail pane must state rather than
    /// render as an empty dependency list (spec §13.13).
    pub fn selected_package(&self) -> Option<PkgIdx> {
        match self.current()? {
            Item::Package(p) => Some(p),
            Item::App(i) => self.package_of(&self.state.catalog.apps[i].packages),
            Item::Tool(i) => self.package_of(&self.state.catalog.tools[i].packages),
        }
    }

    fn package_of(&self, packages: &[String]) -> Option<PkgIdx> {
        self.state.graph.index_of(packages.first()?)
    }

    /// The related packages shown in the detail pane, in a stable order:
    /// dependencies, then reverse dependencies, then optional.
    pub fn related(&self) -> Vec<Related> {
        let Some(pkg) = self.selected_package() else {
            return Vec::new();
        };
        let g = &self.state.graph;
        let mut out: Vec<Related> = Vec::new();

        for &d in g.depends_on(pkg) {
            out.push(Related {
                pkg: d,
                kind: RelationKind::DependsOn,
                note: None,
            });
        }
        for &r in g.required_by(pkg) {
            out.push(Related {
                pkg: r,
                kind: RelationKind::RequiredBy,
                note: None,
            });
        }
        for &o in g.optional_for(pkg) {
            // Surface the reason pacman stores: it says what breaks.
            let note = self.state.db.packages[o as usize]
                .optdepends
                .iter()
                .find(|od| g.index_of(&od.dep.name) == Some(pkg))
                .and_then(|od| od.reason.clone());
            out.push(Related {
                pkg: o,
                kind: RelationKind::Optional,
                note,
            });
        }
        out
    }

    /// The cached removal simulation for the current selection.
    pub fn impact(&mut self) -> Option<&RemovalPlan> {
        let pkg = self.selected_package()?;
        if self.impact.as_ref().map(|(p, _)| *p) != Some(pkg) {
            let plan = self.state.graph.plan_removal(&[pkg]);
            self.impact = Some((pkg, plan));
        }
        self.impact.as_ref().map(|(_, p)| p)
    }

    /// Applications that would disappear if `plan` were carried out.
    ///
    /// Naming the *applications* is the point: "this will also remove GIMP" is
    /// meaningful in a way that "this will also remove gegl" is not (spec §6.3).
    pub fn apps_lost(&self, plan: &RemovalPlan) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for pkg in plan.all_removed() {
            let name = self.state.graph.name(pkg);
            if let Some(apps) = self.apps_by_package.get(name) {
                for &a in apps {
                    let app = &self.state.catalog.apps[a];
                    if !out.contains(&app.name.as_str()) {
                        out.push(&app.name);
                    }
                }
            }
        }
        out
    }

    fn move_selection(&mut self, delta: isize, page: usize) {
        let len = match self.focus {
            Focus::List => self.rows.len(),
            // Counts the removal action too, so Up from the first relationship
            // reaches it.
            Focus::Related => self.related_rows().len(),
        };
        if len == 0 {
            return;
        }
        let cur = match self.focus {
            Focus::List => self.selection(),
            Focus::Related => self.related_selected,
        } as isize;

        let step = delta * page.max(1) as isize;
        let next = (cur + step).clamp(0, len as isize - 1) as usize;

        match self.focus {
            Focus::List => self.set_selection(next),
            Focus::Related => self.related_selected = next,
        }
    }

    /// Jumps to a package, remembering where we came from.
    fn jump_to(&mut self, pkg: PkgIdx) {
        self.history
            .push((self.view, self.selection(), self.query.clone()));
        self.view = View::Dependencies;
        self.query.clear();
        self.rebuild_rows();

        if let Some(row) = self.rows.iter().position(|r| *r == Item::Package(pkg)) {
            self.set_selection(row);
        } else {
            // The target is not a dependency (it may be explicit), so widen the
            // view rather than silently doing nothing.
            self.view = View::Dependencies;
            self.rows = self
                .state
                .db
                .packages
                .iter()
                .enumerate()
                .map(|(i, _)| Item::Package(i as u32))
                .collect();
            if let Some(row) = self.rows.iter().position(|r| *r == Item::Package(pkg)) {
                self.set_selection(row);
            }
        }
        self.focus = Focus::List;
    }

    fn go_back(&mut self) -> bool {
        let Some((view, sel, query)) = self.history.pop() else {
            return false;
        };
        self.view = view;
        self.query = query;
        self.rebuild_rows();
        self.set_selection(sel.min(self.rows.len().saturating_sub(1)));
        self.focus = Focus::List;
        true
    }

    fn switch_view(&mut self, view: View) {
        self.view = view;
        self.focus = Focus::List;
        self.impact = None;
        self.rebuild_rows();
    }

    /// Rows of the relationships pane: the removal action, then relationships.
    pub fn related_rows(&self) -> Vec<RelatedRow> {
        let mut rows = vec![RelatedRow::RemoveAction];
        rows.extend(self.related().into_iter().map(RelatedRow::Relation));
        rows
    }

    /// Opens the removal dialog for the current selection.
    fn open_removal(&mut self) {
        let Some(pkg) = self.selected_package() else {
            return;
        };
        let request = RemovalRequest::build(
            &self.state.graph,
            &self.denylist,
            vec![pkg],
            RemovalMode::WithUnusedDeps,
            &self.app_package_names,
            &self.apps_named_by_package,
        );
        let word = self.state.graph.name(pkg).to_string();
        self.dialog = Some(RemovalDialog::new(request, word));
    }

    /// Opens the dialog for a bulk orphan cleanup.
    fn open_orphan_cleanup(&mut self) {
        // Only the orphans pacman itself reports (spec decision): our extra
        // reachability findings stay individually removable.
        let reported = crate::ops::exec::dry_run(&["-Qdtq".to_string()]).unwrap_or_default();
        let targets = removal::bulk_orphan_targets(&self.state.graph, &reported);
        if targets.is_empty() {
            return;
        }
        let request = RemovalRequest::build(
            &self.state.graph,
            &self.denylist,
            targets,
            RemovalMode::WithUnusedDeps,
            &self.app_package_names,
            &self.apps_named_by_package,
        );
        self.dialog = Some(RemovalDialog::new(request, "remove".to_string()));
    }

    /// Rebuilds the pending request after the mode changes, so the preview
    /// always describes the mode actually selected.
    fn refresh_dialog_request(&mut self) {
        let Some(d) = &self.dialog else { return };
        let (targets, mode) = (d.request.targets.clone(), d.mode());
        let request = RemovalRequest::build(
            &self.state.graph,
            &self.denylist,
            targets,
            mode,
            &self.app_package_names,
            &self.apps_named_by_package,
        );
        if let Some(d) = &mut self.dialog {
            d.request = request;
        }
    }

    /// Advances the dialog past confirmation: verify against pacman, then
    /// authenticate, then run.
    fn confirm_removal(&mut self) {
        let Some(d) = &self.dialog else { return };
        if d.request.is_blocked() {
            return;
        }

        // Dangerous removals need the name typed before anything else happens.
        if d.request.risk.needs_typed_confirmation() && !matches!(d.stage, Stage::TypeToConfirm) {
            if let Some(d) = &mut self.dialog {
                d.stage = Stage::TypeToConfirm;
            }
            return;
        }
        if matches!(d.stage, Stage::TypeToConfirm) && !d.confirmation_satisfied() {
            return;
        }

        // The last gate: pacman's own answer must match ours.
        match removal::verify_against_pacman(&self.dialog.as_ref().unwrap().request, &self.state.graph) {
            Err(e) => {
                if let Some(d) = &mut self.dialog {
                    d.error = Some(e);
                    d.stage = Stage::Done { success: false };
                }
                return;
            }
            Ok(_) => {}
        }

        match removal::auth_stage() {
            crate::ops::exec::AuthState::Ready => self.start_removal(),
            crate::ops::exec::AuthState::NeedsPassword => {
                if let Some(d) = &mut self.dialog {
                    d.stage = Stage::Password;
                    d.password.clear();
                }
            }
            crate::ops::exec::AuthState::Unavailable => {
                if let Some(d) = &mut self.dialog {
                    d.error = Some("sudo is not available on this system".into());
                    d.stage = Stage::Done { success: false };
                }
            }
        }
    }

    fn start_removal(&mut self) {
        let Some(d) = &mut self.dialog else { return };
        let graph = &self.state.graph;
        let names: Vec<String> = d
            .request
            .targets
            .iter()
            .map(|&t| graph.name(t).to_string())
            .collect();
        // Exact versions, so an offline reinstall from the package cache is
        // possible later (spec §6.5).
        let versions: Vec<(String, String)> = d
            .request
            .plan
            .all_removed()
            .iter()
            .map(|&p| {
                let pkg = &graph.db.packages[p as usize];
                (pkg.name.clone(), pkg.version.clone())
            })
            .collect();

        let (tx, rx) = std::sync::mpsc::channel();
        removal::spawn(names, versions, d.mode(), d.snapshot, tx);
        d.receiver = Some(rx);
        d.stage = Stage::Running;
        d.output.clear();
    }

    /// Drains streamed output. Called once per tick, never during a render.
    pub fn pump_output(&mut self) {
        let Some(d) = &mut self.dialog else { return };
        let Some(rx) = &d.receiver else { return };

        let mut finished: Option<bool> = None;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                crate::ops::exec::Output::Line(l) => d.output.push(l),
                crate::ops::exec::Output::Finished { success, .. } => finished = Some(success),
                crate::ops::exec::Output::Failed(e) => {
                    d.error = Some(e);
                    finished = Some(false);
                }
            }
        }
        if let Some(success) = finished {
            d.stage = Stage::Done { success };
            d.receiver = None;
            if success {
                self.needs_reload = true;
            }
        }
    }

    fn handle_dialog_key(&mut self, key: KeyEvent) {
        let Some(d) = &mut self.dialog else { return };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match &d.stage {
            Stage::Confirm => match key.code {
                KeyCode::Esc | KeyCode::Left => self.dialog = None,
                KeyCode::Up => {
                    d.mode_index = d.mode_index.saturating_sub(1);
                    self.refresh_dialog_request();
                }
                KeyCode::Down => {
                    d.mode_index = (d.mode_index + 1).min(RemovalMode::ALL.len() - 1);
                    self.refresh_dialog_request();
                }
                KeyCode::Char('s') if ctrl => d.snapshot = !d.snapshot,
                KeyCode::Enter | KeyCode::Right => self.confirm_removal(),
                _ => {}
            },
            Stage::TypeToConfirm => match key.code {
                KeyCode::Esc => self.dialog = None,
                KeyCode::Backspace => {
                    d.typed.pop();
                }
                KeyCode::Char(c) if !ctrl => d.typed.push(c),
                KeyCode::Enter => self.confirm_removal(),
                _ => {}
            },
            Stage::Password => match key.code {
                KeyCode::Esc => self.dialog = None,
                KeyCode::Backspace => {
                    d.password.pop();
                }
                KeyCode::Char(c) if !ctrl => d.password.push(c),
                KeyCode::Enter => {
                    let pw = std::mem::take(&mut d.password);
                    if removal::try_authenticate(pw) {
                        self.start_removal();
                    } else if let Some(d) = &mut self.dialog {
                        d.error = Some("authentication failed".into());
                    }
                }
                _ => {}
            },
            // Output is streaming; only quitting the dialog is offered, and
            // only once it has finished.
            Stage::Running => {}
            Stage::Done { .. } => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Left => self.dialog = None,
                _ => {}
            },
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.dialog.is_some() {
            self.handle_dialog_key(key);
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // The search field swallows most keys while active.
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                    self.rebuild_rows();
                }
                KeyCode::Enter | KeyCode::Down => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                    self.rebuild_rows();
                }
                KeyCode::Char(c) if !ctrl => {
                    self.query.push(c);
                    self.rebuild_rows();
                }
                _ => {}
            }
            return;
        }

        if self.show_help {
            // Any key dismisses help, so it can never trap a confused user.
            self.show_help = false;
            return;
        }

        match key.code {
            KeyCode::Char('q') if ctrl => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::F(1) => self.show_help = true,

            // Views on the number row. F-keys kept as aliases.
            KeyCode::Char('1') => self.switch_view(View::Apps),
            KeyCode::Char('2') => self.switch_view(View::Tools),
            KeyCode::Char('3') => self.switch_view(View::Dependencies),
            KeyCode::Char('4') => self.switch_view(View::Orphans),
            KeyCode::F(2) => self.switch_view(View::Apps),
            KeyCode::F(3) => self.switch_view(View::Tools),
            KeyCode::F(4) => self.switch_view(View::Dependencies),
            KeyCode::F(6) => self.switch_view(View::Orphans),
            KeyCode::F(5) => self.needs_reload = true,

            // Search is explicit now: typing no longer starts it, so the number
            // row stays available for view switching.
            KeyCode::Char('f') if ctrl => {
                self.searching = true;
                self.query.clear();
            }

            KeyCode::Delete => self.open_removal(),
            // Bulk orphan cleanup, offered only where it makes sense.
            KeyCode::Char('c') if self.view == View::Orphans => self.open_orphan_cleanup(),

            KeyCode::Tab => self.toggle_focus(),
            KeyCode::Up => self.move_selection(-1, 1),
            KeyCode::Down => self.move_selection(1, 1),
            KeyCode::PageUp => self.move_selection(-1, 10),
            KeyCode::PageDown => self.move_selection(1, 10),
            KeyCode::Home => self.move_selection(-1, usize::MAX / 4),
            KeyCode::End => self.move_selection(1, usize::MAX / 4),

            // Right/Enter descends: list → relationships → the selected package.
            KeyCode::Right | KeyCode::Enter => self.descend(),
            // Left/Backspace ascends: relationships → list → wherever we came from.
            KeyCode::Left | KeyCode::Backspace => self.ascend(),

            KeyCode::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.rebuild_rows();
                } else {
                    self.ascend();
                }
            }

            // The -Qdt / -Qdtt distinction, exposed rather than hidden. These are
            // different safety levels and the user is entitled to both.
            KeyCode::Char(' ') if self.view == View::Orphans => {
                self.orphan_mode = match self.orphan_mode {
                    OrphanMode::Conservative => OrphanMode::Aggressive,
                    OrphanMode::Aggressive => OrphanMode::Conservative,
                };
                self.rebuild_rows();
            }
            _ => {}
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::List => {
                self.related_selected = 1.min(self.related_rows().len().saturating_sub(1));
                Focus::Related
            }
            Focus::Related => Focus::List,
        };
    }

    /// Right / Enter: move one step deeper.
    fn descend(&mut self) {
        match self.focus {
            Focus::List => {
                // Land on the first relationship rather than the removal action,
                // so descending never puts a destructive option under the cursor.
                self.related_selected = 1.min(self.related_rows().len().saturating_sub(1));
                self.focus = Focus::Related;
            }
            Focus::Related => match self.related_rows().get(self.related_selected) {
                Some(RelatedRow::RemoveAction) => self.open_removal(),
                Some(RelatedRow::Relation(r)) => {
                    let pkg = r.pkg;
                    self.jump_to(pkg);
                }
                None => {}
            },
        }
    }

    /// Left / Backspace: move one step back out.
    fn ascend(&mut self) {
        match self.focus {
            Focus::Related => self.focus = Focus::List,
            Focus::List => {
                self.go_back();
            }
        }
    }

    /// Label for the source of the selected app, for the detail pane.
    pub fn source_label(source: Source) -> &'static str {
        match source {
            Source::Pacman => "pacman package",
            Source::Flatpak => "Flatpak",
            Source::AppImage => "AppImage (self-contained bundle)",
            Source::Steam => "Steam library entry",
            Source::Unowned => "unknown origin",
        }
    }
}

/// Runs the UI until the user quits.
pub fn run(state: SystemState) -> anyhow::Result<()> {
    let (mut terminal, guard) = term::init()?;
    let mut ui = Ui::new(state, guard.enhanced_keys);

    while !ui.should_quit {
        terminal.draw(|f| render::draw(f, &mut ui))?;

        // A timeout rather than a blocking read, so the lock-file banner can
        // recover on its own when another pacman finishes.
        if event::poll(std::time::Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if key.is_press() => ui.handle_key(key),
                Event::Resize(_, _) => {}
                _ => {}
            }
        } else {
            ui.state.db_locked = crate::state::is_db_locked();
        }
    }

    drop(guard);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_titles_carry_their_binding() {
        // The hint bar is the discoverability mechanism; a view whose key is not
        // shown may as well not have one. Views live on the number row so the
        // F-keys keep their conventional meanings (F1 help, F5 refresh).
        for (i, v) in View::ALL.iter().enumerate() {
            let expected = char::from_digit(i as u32 + 1, 10).unwrap();
            assert!(
                v.title().starts_with(expected),
                "{:?} should start with {expected}",
                v.title()
            );
        }
    }

    #[test]
    fn every_source_has_a_human_label() {
        for s in [
            Source::Pacman,
            Source::Flatpak,
            Source::AppImage,
            Source::Steam,
            Source::Unowned,
        ] {
            let l = Ui::source_label(s);
            assert!(!l.is_empty());
        }
        // The AppImage label must explain the absent dependency list.
        assert!(Ui::source_label(Source::AppImage).contains("self-contained"));
    }
}
