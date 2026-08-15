//! The TUI: state, event handling, and the four M1 views.
//!
//! Bindings are CUA (spec §8, decided): arrows and Tab to move, F-keys to switch
//! view, Ctrl+F to search, F1 for help, Esc to back out, Ctrl+Q to quit. No
//! modal `hjkl` navigation — the audience is a user who does not want to learn
//! a text editor to see what is installed.
//!
//! The key hint bar is always visible. Discoverability *is* the noob protection
//! the spec asks for; a hidden binding may as well not exist.

pub mod render;
pub mod term;

use std::collections::HashMap;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use crate::apps::Source;
use crate::data::graph::{OrphanMode, PkgIdx, RemovalPlan};
use crate::data::local::Reason;
use crate::state::SystemState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum View {
    Apps,
    Tools,
    Dependencies,
    Orphans,
}

impl View {
    pub const ALL: [View; 4] = [View::Apps, View::Tools, View::Dependencies, View::Orphans];

    pub fn title(&self) -> &'static str {
        match self {
            View::Apps => "F2 Apps",
            View::Tools => "F3 Tools",
            View::Dependencies => "F4 Dependencies",
            View::Orphans => "F6 Orphans",
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
}

impl Ui {
    pub fn new(state: SystemState, enhanced_keys: bool) -> Self {
        let mut apps_by_package: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, app) in state.catalog.apps.iter().enumerate() {
            for p in &app.packages {
                apps_by_package.entry(p.clone()).or_default().push(i);
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
            Focus::Related => self.related().len(),
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

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // The search field swallows most keys while active.
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                    self.rebuild_rows();
                }
                KeyCode::Enter => self.searching = false,
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
            KeyCode::F(2) => self.switch_view(View::Apps),
            KeyCode::F(3) => self.switch_view(View::Tools),
            KeyCode::F(4) => self.switch_view(View::Dependencies),
            KeyCode::F(6) => self.switch_view(View::Orphans),
            KeyCode::F(5) => {} // Refresh lands with the background reload.

            KeyCode::Char('f') if ctrl => {
                self.searching = true;
                self.query.clear();
            }
            // Typing in a list view starts a search, as the spec suggests.
            KeyCode::Char(c) if !ctrl && !c.is_whitespace() => {
                self.searching = true;
                self.query.clear();
                self.query.push(c);
                self.rebuild_rows();
            }

            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::List if !self.related().is_empty() => Focus::Related,
                    _ => Focus::List,
                };
            }
            KeyCode::Up => self.move_selection(-1, 1),
            KeyCode::Down => self.move_selection(1, 1),
            KeyCode::PageUp => self.move_selection(-1, 10),
            KeyCode::PageDown => self.move_selection(1, 10),
            KeyCode::Home => self.move_selection(-1, usize::MAX / 4),
            KeyCode::End => self.move_selection(1, usize::MAX / 4),

            KeyCode::Enter => {
                if self.focus == Focus::Related {
                    if let Some(r) = self.related().get(self.related_selected) {
                        let pkg = r.pkg;
                        self.jump_to(pkg);
                    }
                }
            }
            KeyCode::Backspace => {
                self.go_back();
            }
            KeyCode::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.rebuild_rows();
                } else if self.focus == Focus::Related {
                    self.focus = Focus::List;
                } else {
                    self.go_back();
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
        // shown may as well not have one.
        for v in View::ALL {
            assert!(v.title().starts_with('F'), "{:?}", v.title());
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
