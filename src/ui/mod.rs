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
    /// Search and install, across repositories and the AUR.
    Search,
}

impl View {
    pub const ALL: [View; 5] = [
        View::Apps,
        View::Tools,
        View::Dependencies,
        View::Orphans,
        View::Search,
    ];

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
            View::Search => "5 Search",
        }
    }

    /// The search view is a text field first and a list second, so typing goes
    /// straight into the query there rather than needing Ctrl+F.
    pub fn types_to_search(&self) -> bool {
        matches!(self, View::Search)
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
    /// Index into the current search results.
    Result(usize),
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

/// Indexes computed from a `SystemState`, grouped so a reload can replace them
/// all at once and none can be forgotten.
struct Derived {
    apps_by_package: HashMap<String, Vec<usize>>,
    denylist: Denylist,
    app_package_names: std::collections::HashSet<String>,
    apps_named_by_package: HashMap<String, Vec<String>>,
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
    /// A transient message shown in the hint bar, so a keypress that cannot do
    /// anything explains itself instead of appearing broken.
    pub notice: Option<String>,
    /// Terminal graphics backend, probed once at startup.
    pub picker: Option<ratatui_image::picker::Picker>,
    /// Repository databases, loaded in the background: ~260 ms that nothing in
    /// the other views needs on the first frame.
    pub sync: Option<crate::data::sync::SyncDb>,
    sync_rx: Option<std::sync::mpsc::Receiver<crate::data::sync::SyncDb>>,
    /// The AUR package index, downloaded in the background on first use.
    pub aur: Option<crate::data::aur::AurIndex>,
    aur_rx: Option<std::sync::mpsc::Receiver<anyhow::Result<crate::data::aur::AurIndex>>>,
    pub aur_state: crate::data::aur::AurState,
    searcher: crate::data::search::Searcher,
    /// Results for the current query, recomputed when it changes.
    pub results: Vec<crate::data::search::Hit>,
    /// In-flight background reload, if any.
    reload_rx: Option<std::sync::mpsc::Receiver<anyhow::Result<SystemState>>>,
    /// Decoded icon for the current selection, with the key it was built for.
    /// Rebuilt only when the selection changes: decoding on every frame would
    /// put file I/O in the render loop.
    icon: Option<(String, ratatui_image::protocol::StatefulProtocol)>,
}

impl Ui {
    /// Everything derived from a `SystemState`, rebuilt whenever it is replaced.
    fn derive(state: &SystemState) -> Derived {
        let mut apps_by_package: HashMap<String, Vec<usize>> = HashMap::new();
        let mut apps_named_by_package: HashMap<String, Vec<String>> = HashMap::new();
        for (i, app) in state.catalog.apps.iter().enumerate() {
            for p in &app.packages {
                apps_by_package.entry(p.clone()).or_default().push(i);
                apps_named_by_package
                    .entry(p.clone())
                    .or_default()
                    .push(app.name.clone());
            }
        }
        let denylist = Denylist::build(&state.graph);
        let app_package_names: std::collections::HashSet<String> = state
            .catalog
            .apps
            .iter()
            .flat_map(|a| a.packages.iter().cloned())
            .collect();
        Derived {
            apps_by_package,
            denylist,
            app_package_names,
            apps_named_by_package,
        }
    }

    pub fn new(
        state: SystemState,
        enhanced_keys: bool,
        picker: Option<ratatui_image::picker::Picker>,
    ) -> Self {
        let d = Self::derive(&state);
        let (apps_by_package, denylist, app_package_names, apps_named_by_package) = (
            d.apps_by_package,
            d.denylist,
            d.app_package_names,
            d.apps_named_by_package,
        );

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
            notice: None,
            sync: None,
            sync_rx: None,
            aur: None,
            aur_rx: None,
            aur_state: crate::data::aur::AurState::Absent,
            searcher: crate::data::search::Searcher::new(),
            results: Vec::new(),
            reload_rx: None,
            picker,
            icon: None,
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
            View::Search => {
                self.refresh_results();
                return;
            }
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
            // A search result is only a local package when it is installed;
            // otherwise there is nothing on this system to point at.
            Item::Result(i) => {
                let hit = self.results.get(i)?;
                hit.installed.then(|| self.state.graph.index_of(&hit.name))?
            }
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
        // The query means different things in the two contexts — a filter over
        // installed things, or a search over everything available — so it does
        // not carry across.
        if view.types_to_search() != self.view.types_to_search() {
            self.query.clear();
        }
        // The search view opens ready to type; every other view starts in
        // navigation mode.
        self.searching = view.types_to_search();
        self.view = view;
        self.focus = Focus::List;
        self.impact = None;
        self.rebuild_rows();
    }

    /// Total installed size of everything backing an app.
    ///
    /// Summed across all its packages, so a merged app reports what removing
    /// the whole thing would actually reclaim rather than just its primary
    /// package. Returns `None` when no package backs it — Flatpak, AppImage and
    /// Steam entries have a size, but not one pacman knows, and inventing a
    /// zero would read as "this takes no space".
    pub fn app_size(&self, app: &crate::apps::App) -> Option<u64> {
        if app.packages.is_empty() {
            return None;
        }
        Some(
            app.packages
                .iter()
                .filter_map(|p| self.state.graph.index_of(p))
                .map(|i| self.state.db.packages[i as usize].size_bytes())
                .sum(),
        )
    }

    /// The decoded icon for the current selection, if any.
    ///
    /// Cached against the selection key so a redraw never touches the disk.
    pub fn icon(&mut self) -> Option<&mut ratatui_image::protocol::StatefulProtocol> {
        let (key, name) = match self.current() {
            Some(Item::App(i)) => {
                let a = &self.state.catalog.apps[i];
                (format!("app:{}", a.name), a.icon.clone())
            }
            Some(Item::Tool(i)) => {
                let a = &self.state.catalog.tools[i];
                (format!("tool:{}", a.name), a.icon.clone())
            }
            _ => return None,
        };

        if self.icon.as_ref().map(|(k, _)| k.as_str()) != Some(key.as_str()) {
            self.icon = None;
            if let (Some(picker), Some(icon)) = (self.picker.as_mut(), name) {
                if let Some(decoded) = crate::apps::icon::resolve(Some(&icon)) {
                    let proto = picker.new_resize_protocol(image::DynamicImage::ImageRgba8(
                        decoded.rgba,
                    ));
                    self.icon = Some((key, proto));
                }
            }
        }
        self.icon.as_mut().map(|(_, p)| p)
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
            // No pacman package backs this. Saying so is essential: a Delete
            // that silently does nothing is indistinguishable from the tool
            // being broken, which is exactly how it was reported.
            self.notice = Some(match self.current() {
                Some(Item::App(i)) => match self.state.catalog.apps[i].source {
                    Source::Flatpak => {
                        "this is a Flatpak — removing Flatpaks is not implemented yet".into()
                    }
                    Source::AppImage => {
                        "this is an AppImage — removing AppImages is not implemented yet".into()
                    }
                    Source::Steam => "Steam owns this — remove it from your Steam library".into(),
                    _ => "no pacman package backs this, so there is nothing to remove".into(),
                },
                _ => "nothing selected to remove".to_string(),
            });
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

    /// Opens the install dialog for the selected search result.
    fn open_install(&mut self) {
        let Some(hit) = self.selected_hit().cloned() else {
            return;
        };
        if hit.installed {
            self.notice = Some(format!("{} is already installed", hit.name));
            return;
        }

        let source = match hit.origin {
            crate::data::search::Origin::Repo => crate::ops::InstallSource::Repo,
            crate::data::search::Origin::Aur => crate::ops::InstallSource::Aur,
        };
        let helper = if source == crate::ops::InstallSource::Aur {
            crate::ops::find_aur_helper()
        } else {
            None
        };

        let mut warnings = Vec::new();
        if source == crate::ops::InstallSource::Aur {
            warnings.push(
                "This builds from source. AUR packages are user-submitted and not reviewed."
                    .to_string(),
            );
            if hit.orphaned {
                warnings.push("Unmaintained: nobody is applying upstream fixes.".into());
            }
            if hit.out_of_date {
                warnings.push("Flagged out of date by users.".into());
            }
            if helper.is_none() {
                warnings.push("No AUR helper found — install paru or yay first.".into());
            }
        }

        self.dialog = Some(RemovalDialog::install(crate::ops::InstallRequest {
            package: hit.name.clone(),
            version: hit.version.clone(),
            source,
            helper,
            warnings,
        }));
    }

    /// Opens the undo dialog for the most recent removal.
    ///
    /// Says why when there is nothing to undo, rather than doing nothing: a
    /// keypress that silently does nothing is indistinguishable from a bug.
    fn open_undo(&mut self) {
        match crate::ops::restore::last_undoable() {
            Some(entry) => {
                let plan = crate::ops::restore::plan_from(&entry);
                self.dialog = Some(RemovalDialog::restore(plan));
            }
            None => self.notice = Some("nothing to undo — no removal has been recorded".into()),
        }
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
        let Some(req) = d.request() else { return };
        let (targets, mode) = (req.targets.clone(), d.mode());
        let request = RemovalRequest::build(
            &self.state.graph,
            &self.denylist,
            targets,
            mode,
            &self.app_package_names,
            &self.apps_named_by_package,
        );
        if let Some(d) = &mut self.dialog {
            d.job = removal::Job::Remove(request);
        }
    }

    /// Advances the dialog past confirmation: verify against pacman, then
    /// authenticate, then run.
    fn confirm_removal(&mut self) {
        let Some(d) = &self.dialog else { return };
        if d.blocked() {
            return;
        }

        // Dangerous removals need the name typed before anything else happens.
        if d.needs_typed_confirmation() && !matches!(d.stage, Stage::TypeToConfirm) {
            if let Some(d) = &mut self.dialog {
                d.stage = Stage::TypeToConfirm;
            }
            return;
        }
        if matches!(d.stage, Stage::TypeToConfirm) && !d.confirmation_satisfied() {
            return;
        }

        // The last gate for a removal: pacman's own answer must match ours.
        // A restore has nothing to reconcile — it installs named files.
        if let Some(req) = self.dialog.as_ref().and_then(|d| d.request()) {
            if let Err(e) = removal::verify_against_pacman(req, &self.state.graph) {
                if let Some(d) = &mut self.dialog {
                    d.error = Some(e);
                    d.stage = Stage::Done { success: false };
                }
                return;
            }
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

        if let Some(request) = d.job.as_install().cloned() {
            let (tx, rx) = std::sync::mpsc::channel();
            removal::spawn_install(request, tx);
            d.receiver = Some(rx);
            d.stage = Stage::Running;
            d.output.clear();
            self.state.db_locked = false;
            return;
        }

        // Restores take the simpler path: files from the local cache, no
        // snapshot, nothing to reconcile.
        if let Some(plan) = d.job.as_restore().cloned() {
            let (tx, rx) = std::sync::mpsc::channel();
            removal::spawn_restore(plan, tx);
            d.receiver = Some(rx);
            d.stage = Stage::Running;
            d.output.clear();
            return;
        }

        let Some(req) = d.job.as_removal() else { return };
        let graph = &self.state.graph;
        let names: Vec<String> = req
            .targets
            .iter()
            .map(|&t| graph.name(t).to_string())
            .collect();
        // Exact versions, so an offline reinstall from the package cache is
        // possible later (spec §6.5).
        let versions: Vec<(String, String)> = req
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
        // The lock is about to be ours; a stale "another pacman is running"
        // banner over our own operation is worse than no banner.
        self.state.db_locked = false;
    }

    /// Kicks off the repository and AUR loads. Neither blocks the first frame.
    pub fn start_background_loads(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(db) = crate::data::sync::SyncDb::load(crate::data::sync::SyncDb::DEFAULT_ROOT)
            {
                let _ = tx.send(db);
            }
        });
        self.sync_rx = Some(rx);

        // A cached index is used immediately; a download only starts when there
        // is none or it has aged out. Search over repositories works either way,
        // so a cold start is degraded rather than blocked.
        match crate::data::aur::AurIndex::load_cached() {
            Some(index) if !index.is_stale() => {
                self.aur = Some(index);
                self.aur_state = crate::data::aur::AurState::Ready;
            }
            cached => {
                self.aur = cached;
                self.aur_state = crate::data::aur::AurState::Downloading;
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(crate::data::aur::AurIndex::fetch());
                });
                self.aur_rx = Some(rx);
            }
        }
    }

    /// Re-runs the search for the current query.
    fn refresh_results(&mut self) {
        self.results = self.searcher.search(
            &self.query,
            self.sync.as_ref(),
            self.aur.as_ref(),
            &self.state.db,
            200,
        );
        self.rows = (0..self.results.len()).map(Item::Result).collect();
        let max = self.rows.len().saturating_sub(1);
        if self.selection() > max {
            self.set_selection(max);
        }
    }

    pub fn selected_hit(&self) -> Option<&crate::data::search::Hit> {
        match self.current()? {
            Item::Result(i) => self.results.get(i),
            _ => None,
        }
    }

    /// Starts rebuilding the system snapshot on a background thread.
    ///
    /// Reloading is the whole point of F5 and the reason a removed package
    /// should stop appearing in the list; doing it on the UI thread would stall
    /// the render loop for the duration of a full rescan.
    pub fn start_reload(&mut self) {
        if self.reload_rx.is_some() {
            return;
        }
        self.needs_reload = false;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(SystemState::load());
        });
        self.reload_rx = Some(rx);
        self.notice = Some("refreshing…".into());
    }

    pub fn is_reloading(&self) -> bool {
        self.reload_rx.is_some()
    }

    /// Swaps in a finished reload, preserving the user's place by name.
    fn finish_reload(&mut self, state: SystemState) {
        // Remember what was selected so the cursor does not jump to the top of
        // the list every refresh.
        let previous = self.current().map(|item| match item {
            Item::App(i) => self.state.catalog.apps[i].name.clone(),
            Item::Tool(i) => self.state.catalog.tools[i].name.clone(),
            Item::Package(p) => self.state.graph.name(p).to_string(),
            Item::Result(i) => self
                .results
                .get(i)
                .map(|h| h.name.clone())
                .unwrap_or_default(),
        });

        let d = Self::derive(&state);
        self.state = state;
        self.apps_by_package = d.apps_by_package;
        self.denylist = d.denylist;
        self.app_package_names = d.app_package_names;
        self.apps_named_by_package = d.apps_named_by_package;

        // Anything cached against the old snapshot is now meaningless.
        self.impact = None;
        self.icon = None;
        self.history.clear();
        self.rebuild_rows();

        if let Some(name) = previous {
            if let Some(row) = self.rows.iter().position(|item| match item {
                Item::App(i) => self.state.catalog.apps[*i].name == name,
                Item::Tool(i) => self.state.catalog.tools[*i].name == name,
                Item::Package(p) => self.state.graph.name(*p) == name,
                Item::Result(i) => self.results.get(*i).is_some_and(|h| h.name == name),
            }) {
                self.set_selection(row);
            } else {
                // It is gone — which after a removal is the correct outcome.
                let max = self.rows.len().saturating_sub(1);
                self.set_selection(self.selection().min(max));
            }
        }
    }

    /// True while a spawned operation is still producing output.
    pub fn operation_running(&self) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|d| matches!(d.stage, Stage::Running))
    }

    /// Drains streamed output. Called once per tick, never during a render.
    pub fn pump_output(&mut self) {
        if let Some(d) = &mut self.dialog {
            d.pump_pkgbuild();
        }
        // Collect a finished reload first, so a refresh started by a removal
        // lands as soon as it is ready.
        if let Some(rx) = &self.reload_rx {
            match rx.try_recv() {
                Ok(Ok(state)) => {
                    self.reload_rx = None;
                    self.finish_reload(state);
                    self.notice = None;
                }
                Ok(Err(e)) => {
                    self.reload_rx = None;
                    self.notice = Some(format!("refresh failed: {e}"));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.reload_rx = None,
            }
        }
        if let Some(rx) = &self.sync_rx {
            if let Ok(db) = rx.try_recv() {
                self.sync = Some(db);
                self.sync_rx = None;
                if self.view == View::Search {
                    self.refresh_results();
                }
            }
        }
        if let Some(rx) = &self.aur_rx {
            match rx.try_recv() {
                Ok(Ok(index)) => {
                    self.aur = Some(index);
                    self.aur_state = crate::data::aur::AurState::Ready;
                    self.aur_rx = None;
                    if self.view == View::Search {
                        self.refresh_results();
                    }
                }
                Ok(Err(_)) => {
                    self.aur_state = crate::data::aur::AurState::Failed;
                    self.aur_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.aur_rx = None,
            }
        }

        if self.needs_reload && self.dialog.is_none() {
            self.start_reload();
        }

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
                // PKGBUILD review, for AUR installs only.
                KeyCode::Char('p') if d.job.as_install().is_some() => {
                    if d.pkgbuild.is_some() {
                        d.pkgbuild = None;
                    } else {
                        d.request_pkgbuild();
                    }
                }
                KeyCode::PageDown if d.pkgbuild.is_some() => {
                    d.pkgbuild_scroll = d.pkgbuild_scroll.saturating_add(10)
                }
                KeyCode::PageUp if d.pkgbuild.is_some() => {
                    d.pkgbuild_scroll = d.pkgbuild_scroll.saturating_sub(10)
                }
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
                    match removal::try_authenticate(pw) {
                        Ok(()) => self.start_removal(),
                        Err(why) => {
                            if let Some(d) = &mut self.dialog {
                                d.error = Some(why);
                            }
                        }
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
        self.notice = None;
        if self.dialog.is_some() {
            self.handle_dialog_key(key);
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // While the query field has focus it takes printable keys — including
        // digits, which would otherwise switch views. Escape releases it, which
        // is the only way back to the view keys in the search view.
        if self.searching {
            match key.code {
                KeyCode::Char('q') if ctrl => self.should_quit = true,
                KeyCode::Char('c') if ctrl => self.should_quit = true,

                // View switching stays reachable without leaving the field.
                KeyCode::Tab => self.cycle_view(1),
                KeyCode::BackTab => self.cycle_view(-1),

                KeyCode::Esc => {
                    if self.view == View::Search {
                        // Keep the results; just stop capturing keystrokes.
                        self.searching = false;
                    } else {
                        self.searching = false;
                        self.query.clear();
                        self.rebuild_rows();
                    }
                }
                KeyCode::Enter => {
                    if self.view == View::Search {
                        self.open_install();
                    } else {
                        self.searching = false;
                    }
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.rebuild_rows();
                }
                KeyCode::Char(c) if !ctrl => {
                    self.query.push(c);
                    self.rebuild_rows();
                }
                KeyCode::Up => self.move_selection(-1, 1),
                KeyCode::Down => self.move_selection(1, 1),
                KeyCode::PageUp => self.move_selection(-1, 10),
                KeyCode::PageDown => self.move_selection(1, 10),
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
            KeyCode::Char('5') => self.switch_view(View::Search),
            KeyCode::F(7) => self.switch_view(View::Search),
            KeyCode::F(5) => self.start_reload(),

            // Search is explicit now: typing no longer starts it, so the number
            // row stays available for view switching.
            KeyCode::Char('f') if ctrl => {
                self.searching = true;
                // In the search view this resumes editing the existing query;
                // elsewhere it starts a fresh filter.
                if self.view != View::Search {
                    self.query.clear();
                }
            }

            KeyCode::Delete => self.open_removal(),
            // Undo the last removal, restoring from the package cache.
            KeyCode::Char('z') if ctrl => self.open_undo(),
            // Bulk orphan cleanup, offered only where it makes sense.
            KeyCode::Char('c') if self.view == View::Orphans => self.open_orphan_cleanup(),

            // Tab walks the views. Pane focus is already covered by Right and
            // Left, which is a more natural fit for moving deeper and back.
            KeyCode::Tab => self.cycle_view(1),
            KeyCode::BackTab => self.cycle_view(-1),
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

    /// Moves to the next or previous view, wrapping around.
    fn cycle_view(&mut self, delta: isize) {
        let count = View::ALL.len() as isize;
        let current = View::ALL
            .iter()
            .position(|v| *v == self.view)
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(count) as usize;
        self.switch_view(View::ALL[next]);
    }

    #[allow(dead_code)]
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
        if self.view == View::Search && self.focus == Focus::List {
            self.open_install();
            return;
        }
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

/// Determines how to render images, without disturbing terminal input.
///
/// **The stdio query is only used on terminals that advertise graphics
/// support.** It writes an escape sequence and waits for a reply on stdin; a
/// terminal that never answers — Konsole, and anything under tmux — leaves the
/// first keystroke of the session swallowed, so the Delete that should open the
/// removal dialog silently does nothing. That is a far worse failure than
/// rendering icons as half-blocks, which looks fine and works everywhere.
fn probe_picker() -> Option<ratatui_image::picker::Picker> {
    use ratatui_image::picker::Picker;

    if supports_graphics_protocol() {
        if let Ok(p) = Picker::from_query_stdio() {
            return Some(p);
        }
    }
    // The dedicated half-blocks constructor: no terminal query, so no chance
    // of consuming a keystroke.
    Some(Picker::halfblocks())
}

/// Whether the terminal is one known to implement kitty/sixel/iTerm graphics.
///
/// Deliberately an allowlist. Guessing wrong costs a swallowed keystroke, and
/// the fallback renderer is perfectly usable.
fn supports_graphics_protocol() -> bool {
    // tmux does not forward the query reply, whatever the outer terminal is.
    if std::env::var_os("TMUX").is_some() {
        return false;
    }
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return true;
    }
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();
    ["kitty", "ghostty", "foot", "wezterm", "iterm"]
        .iter()
        .any(|t| term.contains(t) || program.contains(t))
}

/// Runs the UI until the user quits.
pub fn run(state: SystemState) -> anyhow::Result<()> {
    // Warm the icon index off the render loop. It is only needed for entries
    // the fast path cannot place, but building it costs ~600 ms, and paying
    // that on the first arrow key over a Steam shortcut is a visible stall.
    std::thread::spawn(crate::apps::icon::warm_index);

    // Probe the terminal's graphics support **before** entering raw mode.
    //
    // The probe writes an escape sequence and reads the reply straight from
    // stdin. Doing that once the event loop owns input means the two compete
    // for the same bytes, and the observed symptom is keystrokes going missing
    // for the first second or so after startup — including the Delete that is
    // supposed to open the removal dialog. Falling back to a fixed font size
    // costs only native graphics protocols; half-blocks still render.
    let picker = probe_picker();

    let (mut terminal, guard) = term::init()?;
    let mut ui = Ui::new(state, guard.enhanced_keys, picker);
    ui.start_background_loads();

    while !ui.should_quit {
        terminal.draw(|f| render::draw(f, &mut ui))?;

        // Drain any running operation's output. Must happen every tick: without
        // it a started removal streams into a channel nobody reads, so the
        // dialog sits on "removing…" forever and the user cannot tell whether
        // anything is happening.
        ui.pump_output();

        // Poll faster while an operation is live so its output appears as it
        // arrives rather than in quarter-second lumps.
        let timeout = if ui.operation_running() {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(250)
        };

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.is_press() => ui.handle_key(key),
                Event::Resize(_, _) => {}
                _ => {}
            }
        } else if !ui.operation_running() {
            // Only when the lock is not ours: during our own removal pacman
            // holds it, and warning the user that "another pacman is running"
            // while they watch our removal run is simply wrong.
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
