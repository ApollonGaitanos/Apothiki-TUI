//! Drawing. Pure functions of `Ui` — no I/O, no computation that could block.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::Frame;

use super::removal::Stage;
use super::{Focus, Item, RelatedRow, RelationKind, Ui, View};
use crate::ops::safety::Risk;
use crate::ops::RemovalMode;
use crate::apps::{Evidence, Source};
use crate::data::graph::OrphanMode;
use crate::data::local::Reason;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const WARN: Color = Color::Yellow;
const DANGER: Color = Color::Red;
const OK: Color = Color::Green;

pub fn draw(f: &mut Frame, ui: &mut Ui) {
    let banner = u16::from(ui.state.db_locked);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // tabs
            Constraint::Length(banner), // db.lck banner
            Constraint::Min(3),         // body
            Constraint::Length(1),      // key hints
        ])
        .split(f.area());

    draw_tabs(f, chunks[0], ui);
    if ui.state.db_locked {
        draw_lock_banner(f, chunks[1]);
    }

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(chunks[2]);

    draw_list(f, body[0], ui);
    draw_detail(f, body[1], ui);
    draw_keybar(f, chunks[3], ui);

    if ui.show_help {
        draw_help(f, f.area(), ui);
    }
    if ui.dialog.is_some() {
        draw_removal(f, f.area(), ui);
    }
}

fn centred(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// The removal dialog.
///
/// Shows what will happen before asking whether to do it, and names the
/// *applications* at stake rather than only package names (spec §6.3).
fn draw_removal(f: &mut Frame, area: Rect, ui: &Ui) {
    let Some(d) = &ui.dialog else { return };
    let graph = &ui.state.graph;
    let popup = centred(area, 86, 28);

    let mut lines: Vec<Line> = Vec::new();
    let title = match &d.stage {
        Stage::Running => " removing… ",
        Stage::Done { success: true } => " done ",
        Stage::Done { success: false } => " failed ",
        _ => " remove ",
    };

    match &d.stage {
        Stage::Confirm | Stage::TypeToConfirm => {
            // Restoring is a different operation with a different shape: no
            // modes, no cascade, no typed confirmation.
            if let Some(plan) = d.job.as_restore() {
                draw_restore_confirm(f, popup, plan);
                return;
            }
            let Some(req) = d.job.as_removal() else { return };
            let names = req.package_names(graph);
            lines.push(Line::styled(
                names.join(", "),
                Style::default().add_modifier(Modifier::BOLD).fg(ACCENT),
            ));

            // A blocked removal explains itself and offers nothing. There is no
            // override, by design (spec §6.1).
            if let Some(p) = &req.blocked_by {
                lines.push(Line::raw(""));
                lines.push(Line::styled(p.explain(), Style::default().fg(DANGER)));
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "There is no way to force this, and that is deliberate.",
                    Style::default().fg(DIM),
                ));
                lines.push(Line::styled("Esc to close", Style::default().fg(DIM)));
                render_popup(f, popup, title, lines);
                return;
            }

            if req.plan.is_blocked() {
                let who: Vec<&str> = req
                    .plan
                    .blockers
                    .iter()
                    .map(|(dep, _)| graph.name(*dep))
                    .take(8)
                    .collect();
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!("Still required by: {}", who.join(", ")),
                    Style::default().fg(DANGER),
                ));
                lines.push(Line::styled(
                    "pacman would refuse this removal.",
                    Style::default().fg(DIM),
                ));
                lines.push(Line::styled("Esc to close", Style::default().fg(DIM)));
                render_popup(f, popup, title, lines);
                return;
            }

            lines.push(Line::raw(""));
            for (i, m) in RemovalMode::ALL.iter().enumerate() {
                let selected = i == d.mode_index;
                let marker = if selected { "▸ " } else { "  " };
                lines.push(Line::styled(
                    format!("{marker}{}  ({})", m.label(), m.flags()),
                    if selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(DIM)
                    },
                ));
                if selected {
                    lines.push(Line::styled(
                        format!("    {}", m.detail()),
                        Style::default().fg(DIM),
                    ));
                }
            }

            lines.push(Line::raw(""));
            let total = req.plan.all_removed().len();
            lines.push(Line::raw(format!(
                "{total} packages, {} freed",
                human_size(req.plan.freed_bytes)
            )));
            if !req.apps_lost.is_empty() {
                lines.push(Line::styled(
                    format!("Applications removed: {}", req.apps_lost.join(", ")),
                    Style::default().fg(DANGER),
                ));
            }
            if !req.plan.optdep_losses.is_empty() {
                let who: Vec<&str> = req
                    .plan
                    .optdep_losses
                    .iter()
                    .map(|(dep, _)| graph.name(*dep))
                    .take(5)
                    .collect();
                lines.push(Line::styled(
                    format!("Silently degrades: {}", who.join(", ")),
                    Style::default().fg(WARN),
                ));
            }

            let risk_colour = match req.risk {
                Risk::Safe => OK,
                Risk::Caution => WARN,
                _ => DANGER,
            };
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(
                    "[{}] {}",
                    req.risk.symbol(),
                    super::removal::risk_sentence(req.risk, &req.apps_lost)
                ),
                Style::default().fg(risk_colour),
            ));

            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(
                    "[{}] snapper snapshot first  (Ctrl+S)",
                    if d.snapshot { "x" } else { " " }
                ),
                Style::default().fg(if d.snapshot { OK } else { DIM }),
            ));
            lines.push(Line::styled(
                format!("$ {}", req.command_line(graph)),
                Style::default().fg(DIM),
            ));

            if matches!(d.stage, Stage::TypeToConfirm) {
                use super::removal::ConfirmState;
                let state = d.confirmation_state();
                let colour = match state {
                    ConfirmState::Matches => OK,
                    ConfirmState::Wrong => DANGER,
                    _ => WARN,
                };
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!("Type \"{}\" to confirm:", d.confirm_word),
                    Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::styled(
                    format!("  {}▏", d.typed),
                    Style::default().fg(colour),
                ));
                // Live feedback: without it, the only signal for a typo is
                // Enter appearing to do nothing.
                lines.push(Line::styled(
                    format!("  {}", state.hint(&d.confirm_word)),
                    Style::default().fg(colour),
                ));
            }

            lines.push(Line::raw(""));
            lines.push(Line::styled(
                if req.risk.needs_typed_confirmation() && !matches!(d.stage, Stage::TypeToConfirm) {
                    "↑↓ mode   Enter/→ continue   Esc cancel"
                } else if matches!(d.stage, Stage::TypeToConfirm) {
                    "Enter confirm   Esc cancel"
                } else {
                    "↑↓ mode   Enter/→ remove   Esc cancel"
                }
                .to_string(),
                Style::default().fg(DIM),
            ));
        }
        Stage::Password => {
            lines.push(Line::raw("Administrator password required."));
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!("  {}▏", "•".repeat(d.password.chars().count()))));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Used only to authorise this operation, then discarded.",
                Style::default().fg(DIM),
            ));
            if let Some(e) = &d.error {
                lines.push(Line::styled(e.clone(), Style::default().fg(DANGER)));
            }
            lines.push(Line::styled("Enter confirm   Esc cancel", Style::default().fg(DIM)));
        }
        Stage::Running | Stage::Done { .. } => {
            for l in d.output.iter().rev().take(20).collect::<Vec<_>>().into_iter().rev() {
                lines.push(Line::raw(l.clone()));
            }
            if let Some(e) = &d.error {
                lines.push(Line::styled(e.clone(), Style::default().fg(DANGER)));
            }
            match d.stage {
                Stage::Done { success: true } => {
                    lines.push(Line::styled("Finished.", Style::default().fg(OK)));
                    lines.push(Line::styled("Esc to close", Style::default().fg(DIM)));
                }
                Stage::Done { success: false } => {
                    lines.push(Line::styled(
                        "Nothing was changed.",
                        Style::default().fg(WARN),
                    ));
                    lines.push(Line::styled("Esc to close", Style::default().fg(DIM)));
                }
                _ => {}
            }
        }
    }

    render_popup(f, popup, title, lines);
}

/// The undo dialog.
///
/// States plainly what can and cannot be brought back: a purge deleted config
/// files, and no reinstall returns them.
fn draw_restore_confirm(f: &mut Frame, popup: Rect, plan: &crate::ops::restore::RestorePlan) {
    let mut lines: Vec<Line> = vec![Line::styled(
        "Undo the last removal",
        Style::default().add_modifier(Modifier::BOLD).fg(ACCENT),
    )];
    lines.push(Line::styled(
        format!("removed with {} on {}", plan.operation, format_date(plan.timestamp)),
        Style::default().fg(DIM),
    ));
    lines.push(Line::raw(""));

    if !plan.missing.is_empty() {
        lines.push(Line::styled(
            format!(
                "{} package(s) are no longer in the package cache:",
                plan.missing.len()
            ),
            Style::default().fg(DANGER),
        ));
        for (n, v) in plan.missing.iter().take(6) {
            lines.push(Line::styled(format!("  {n} {v}"), Style::default().fg(DIM)));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "A partial restore would leave the system in a state neither you",
            Style::default().fg(DIM),
        ));
        lines.push(Line::styled(
            "nor this tool could describe, so it is not offered.",
            Style::default().fg(DIM),
        ));
        lines.push(Line::styled(
            "Reinstall from the repositories instead, or roll back the snapshot.",
            Style::default().fg(DIM),
        ));
        lines.push(Line::styled("Esc to close", Style::default().fg(DIM)));
        render_popup(f, popup, " undo ", lines);
        return;
    }

    lines.push(Line::raw(format!(
        "Reinstall {} package(s) from the local cache:",
        plan.available.len()
    )));
    for (n, v, _) in plan.available.iter().take(10) {
        lines.push(Line::raw(format!("  {n} {v}")));
    }
    if plan.available.len() > 10 {
        lines.push(Line::styled(
            format!("  … and {} more", plan.available.len() - 10),
            Style::default().fg(DIM),
        ));
    }

    if plan.configs_were_purged {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "That removal was a purge: config files were deleted and cannot",
            Style::default().fg(WARN),
        ));
        lines.push(Line::styled(
            "be restored by reinstalling. Only the packages come back.",
            Style::default().fg(WARN),
        ));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("$ {}", plan.command_line()),
        Style::default().fg(DIM),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Enter restore   Esc cancel",
        Style::default().fg(DIM),
    ));
    render_popup(f, popup, " undo ", lines);
}

fn render_popup(f: &mut Frame, popup: Rect, title: &str, lines: Vec<Line>) {
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DANGER))
                .title(title.to_string()),
        ),
        popup,
    );
}

fn draw_tabs(f: &mut Frame, area: Rect, ui: &Ui) {
    let titles: Vec<Line> = View::ALL
        .iter()
        .map(|v| {
            let count = match v {
                View::Apps => ui.state.catalog.apps.len(),
                View::Tools => ui.state.catalog.tools.len(),
                View::Dependencies => ui
                    .state
                    .db
                    .packages
                    .iter()
                    .filter(|p| p.reason == Reason::Dependency)
                    .count(),
                View::Orphans => ui.state.graph.orphans(ui.orphan_mode).len(),
            };
            Line::from(format!(" {} ({count}) ", v.title()))
        })
        .collect();

    let selected = View::ALL.iter().position(|v| *v == ui.view).unwrap_or(0);
    f.render_widget(
        Tabs::new(titles)
            .select(selected)
            .style(Style::default().fg(DIM))
            .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
            .divider(""),
        area,
    );
}

/// A concurrent pacman transaction must be visible, not discovered through a
/// confusing failure later (spec §5.1).
fn draw_lock_banner(f: &mut Frame, area: Rect) {
    f.render_widget(
        Paragraph::new(" another pacman process is running — data may be out of date ")
            .style(Style::default().fg(Color::Black).bg(WARN)),
        area,
    );
}

fn draw_list(f: &mut Frame, area: Rect, ui: &mut Ui) {
    let focused = ui.focus == Focus::List;
    let title = if ui.searching || !ui.query.is_empty() {
        format!(" search: {}▏ ({} matches) ", ui.query, ui.rows().len())
    } else {
        format!(" {} ", ui.rows().len())
    };

    let items: Vec<ListItem> = ui
        .rows()
        .iter()
        .map(|item| ListItem::new(row_line(ui, *item)))
        .collect();

    let mut state = ListState::default();
    state.select(Some(ui.selection()));

    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if focused { ACCENT } else { DIM }))
                    .title(title),
            )
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(if focused { Color::DarkGray } else { Color::Reset }),
            )
            .highlight_symbol("▌"),
        area,
        &mut state,
    );
}

fn row_line<'a>(ui: &'a Ui, item: Item) -> Line<'a> {
    match item {
        Item::App(i) => {
            let app = &ui.state.catalog.apps[i];
            let (tag, colour) = source_tag(app.source);
            Line::from(vec![
                Span::styled(format!("{tag:<5} "), Style::default().fg(colour)),
                Span::raw(app.name.clone()),
                Span::styled(size_suffix(ui, app), Style::default().fg(DIM)),
            ])
        }
        Item::Tool(i) => {
            let tool = &ui.state.catalog.tools[i];
            Line::from(vec![
                Span::raw(tool.name.clone()),
                Span::styled(size_suffix(ui, tool), Style::default().fg(DIM)),
            ])
        }
        Item::Package(p) => {
            let pkg = &ui.state.db.packages[p as usize];
            Line::from(vec![
                Span::raw(pkg.name.clone()),
                Span::styled(
                    format!("  {}", human_size(pkg.size_bytes())),
                    Style::default().fg(DIM),
                ),
            ])
        }
    }
}

/// Trailing size for a list row, empty when pacman does not own the app.
fn size_suffix(ui: &Ui, app: &crate::apps::App) -> String {
    match ui.app_size(app) {
        Some(bytes) => format!("  {}", human_size(bytes)),
        None => String::new(),
    }
}

fn source_tag(source: Source) -> (&'static str, Color) {
    match source {
        Source::Pacman => ("pkg", Color::Reset),
        Source::Flatpak => ("flat", Color::Blue),
        Source::AppImage => ("img", Color::Magenta),
        Source::Steam => ("steam", DIM),
        Source::Unowned => ("?", WARN),
    }
}

fn draw_detail(f: &mut Frame, area: Rect, ui: &mut Ui) {
    let mut lines: Vec<Line> = Vec::new();

    match ui.current() {
        None => lines.push(Line::styled("nothing selected", Style::default().fg(DIM))),
        Some(item) => {
            detail_header(ui, item, &mut lines);
            detail_package(ui, &mut lines);
        }
    }

    // Impact preview: display only. M1 performs no removals of any kind.
    //
    // It gets its own pane with a reserved height rather than trailing the
    // details text. Appended to a long detail body it fell below the fold
    // exactly for the packages with the most to say — and the answer to "what
    // happens if I delete this" is the one thing that must never be the part
    // that gets clipped.
    let plan_lines = impact_lines(ui);
    let impact_height = (plan_lines.len() as u16 + 2).clamp(3, 9);

    let related_focused = ui.focus == Focus::Related;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Length(impact_height),
            Constraint::Percentage(38),
        ])
        .split(area);

    // Icon beside the text when the app has one. The block is drawn first so
    // both halves sit inside one border.
    let details_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM))
        .title(" details ");
    let inner = details_block.inner(chunks[0]);
    f.render_widget(details_block, chunks[0]);

    let has_icon = ui.icon().is_some();
    let text_area = if has_icon {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(12), Constraint::Min(10)])
            .split(inner);
        if let Some(proto) = ui.icon() {
            // Height-limited so a tall icon cannot push the text off screen,
            // and two columns narrower than its cell to leave a gutter — the
            // half-block renderer fills its rect edge to edge, so without this
            // the icon runs straight into the app name.
            let icon_area = Rect {
                width: split[0].width.saturating_sub(2),
                height: split[0].height.min(6),
                ..split[0]
            };
            f.render_stateful_widget(
                ratatui_image::StatefulImage::default(),
                icon_area,
                proto,
            );
        }
        split[1]
    } else {
        inner
    };

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        text_area,
    );

    f.render_widget(
        Paragraph::new(plan_lines)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(DIM))
                    .title(" impact preview (nothing is removed) "),
            ),
        chunks[1],
    );

    // The three relationship kinds are kept visually separate: conflating
    // "depends on" with "required by" is the single most confusing thing a
    // package tool can do (spec §5.2). The removal action sits above them.
    let rows = ui.related_rows();
    let removable = ui.selected_package().is_some();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            RelatedRow::RemoveAction => {
                let (text, colour) = match ui.selected_package() {
                    Some(p) if ui.denylist.is_protected(p) => {
                        ("protected — this cannot be removed", DIM)
                    }
                    Some(_) => ("Remove this…  (Del)", DANGER),
                    None => ("no package to remove", DIM),
                };
                ListItem::new(Line::styled(
                    text.to_string(),
                    Style::default().fg(colour).add_modifier(Modifier::BOLD),
                ))
            }
            RelatedRow::Relation(r) => {
                let colour = match r.kind {
                    RelationKind::DependsOn => Color::Reset,
                    RelationKind::RequiredBy => ACCENT,
                    RelationKind::Optional => WARN,
                };
                let mut spans = vec![
                    Span::styled(format!("{:<12} ", r.kind.label()), Style::default().fg(colour)),
                    Span::raw(ui.state.graph.name(r.pkg).to_string()),
                ];
                if let Some(note) = &r.note {
                    spans.push(Span::styled(format!("  — {note}"), Style::default().fg(DIM)));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();

    let _ = removable;
    let title = format!(
        " relationships ({}) — → to open, ← to go back ",
        rows.len().saturating_sub(1)
    );

    let mut state = ListState::default();
    state.select(Some(ui.related_selected.min(rows.len().saturating_sub(1))));

    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if related_focused { ACCENT } else { DIM }))
                    .title(title),
            )
            .highlight_style(Style::default().add_modifier(Modifier::BOLD).bg(
                if related_focused { Color::DarkGray } else { Color::Reset },
            ))
            .highlight_symbol("▌"),
        chunks[2],
        &mut state,
    );
}

fn detail_header(ui: &Ui, item: Item, lines: &mut Vec<Line>) {
    let app = match item {
        Item::App(i) => Some(&ui.state.catalog.apps[i]),
        Item::Tool(i) => Some(&ui.state.catalog.tools[i]),
        Item::Package(_) => None,
    };

    if let Some(app) = app {
        lines.push(Line::styled(
            app.name.clone(),
            Style::default().add_modifier(Modifier::BOLD).fg(ACCENT),
        ));
        if let Some(s) = &app.summary {
            lines.push(Line::raw(s.clone()));
        }
        lines.push(Line::raw(""));
        lines.push(field("source", Ui::source_label(app.source)));
        if !app.categories.is_empty() {
            lines.push(field("categories", &app.categories.join(", ")));
        }
        if let Some(e) = &app.exec {
            lines.push(field("launches", e));
        }
        if !app.packages.is_empty() {
            lines.push(field("packages", &app.packages.join(", ")));
        }

        // Every classification is explainable — the tool must be able to say
        // why it believes what it says (spec §16).
        lines.push(Line::raw(""));
        lines.push(Line::styled("evidence", Style::default().fg(DIM)));
        for e in &app.evidence {
            lines.push(Line::styled(format!("  {}", evidence_text(e)), Style::default().fg(DIM)));
        }

        if app.source == Source::AppImage {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "self-contained bundle — no dependencies to show",
                Style::default().fg(DIM),
            ));
        }
        lines.push(Line::raw(""));
    }
}

fn evidence_text(e: &Evidence) -> String {
    match e {
        Evidence::Metainfo(id) => format!("AppStream component {id}"),
        Evidence::DesktopEntry(id) => format!("desktop entry {id}"),
        Evidence::MergedPackage { package, suffix } => {
            format!("merged {package} (-{suffix} with no launchable of its own)")
        }
        Evidence::TerminalEntry => "desktop entry marked Terminal=true".into(),
        Evidence::ExplicitWithBinary(b) => format!("explicitly installed, provides /usr/bin/{b}"),
        Evidence::ExplicitNoLaunchable => "explicitly installed, no launchable".into(),
        Evidence::Flatpak { id, origin } => format!("flatpak {id} from {origin}"),
        Evidence::AppImageFile(p) => format!("AppImage at {p}"),
        Evidence::SteamShortcut => "launches through Steam".into(),
    }
}

fn detail_package(ui: &Ui, lines: &mut Vec<Line>) {
    let Some(idx) = ui.selected_package() else {
        return;
    };
    let pkg = &ui.state.db.packages[idx as usize];

    lines.push(Line::styled(
        format!("package: {}", pkg.name),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if let Some(d) = &pkg.desc {
        lines.push(Line::raw(d.clone()));
    }
    lines.push(field("version", &pkg.version));

    // On CachyOS the same name exists in both the Arch and CachyOS repos, so
    // origin is load-bearing for most of the list (spec §11).
    lines.push(field(
        "repo",
        pkg.repo.as_deref().unwrap_or("unknown (not recorded)"),
    ));
    lines.push(field("size", &human_size(pkg.size_bytes())));
    lines.push(field(
        "install reason",
        match pkg.reason {
            Reason::Explicit => "explicitly installed",
            Reason::Dependency => "installed as a dependency",
        },
    ));
    if let Some(t) = pkg.install_date {
        lines.push(field("installed", &format_date(t)));
    }
    if !pkg.groups.is_empty() {
        lines.push(field("groups", &pkg.groups.join(", ")));
    }
    if let Some(u) = &pkg.url {
        lines.push(field("url", u));
    }

    let backups = ui.state.index.backups_of(&pkg.name);
    if !backups.is_empty() {
        lines.push(field(
            "config files",
            &format!("{} tracked (would be left as .pacsave)", backups.len()),
        ));
    }
}

fn impact_lines(ui: &mut Ui) -> Vec<Line<'static>> {
    // No backing package means no pacman removal to simulate. Say so: an empty
    // bordered box is the same "looks broken" failure as an empty dependency
    // list (spec §13.13).
    let Some(plan) = ui.impact().cloned() else {
        let msg = match ui.current() {
            Some(Item::App(i)) => match ui.state.catalog.apps[i].source {
                Source::Flatpak => "removed with `flatpak uninstall`, not pacman",
                Source::AppImage => {
                    "removing means deleting the bundle, its desktop entry and icon"
                }
                Source::Steam => "Steam owns this — remove it from your Steam library",
                _ => "no pacman package backs this, so there is nothing to simulate",
            },
            _ => "no pacman package backs this, so there is nothing to simulate",
        };
        return vec![Line::styled(msg, Style::default().fg(DIM))];
    };
    let apps_lost: Vec<String> = ui.apps_lost(&plan).iter().map(|s| s.to_string()).collect();

    let mut lines: Vec<Line<'static>> = Vec::new();

    if plan.is_blocked() {
        // Not an error state: it means the package is load-bearing, which is
        // exactly what the user wants to know before trying.
        let blockers: Vec<&str> = plan
            .blockers
            .iter()
            .map(|(dependent, _)| ui.state.graph.name(*dependent))
            .take(6)
            .collect();
        lines.push(Line::styled(
            format!("blocked — still required by {}", blockers.join(", ")),
            Style::default().fg(DANGER),
        ));
        return lines;
    }

    let total = plan.all_removed().len();
    let colour = if total > 20 {
        DANGER
    } else if total > 5 {
        WARN
    } else {
        OK
    };
    lines.push(Line::styled(
        format!(
            "{total} packages ({} target, {} cascade), {} freed",
            plan.target.len(),
            plan.cascade.len(),
            human_size(plan.freed_bytes)
        ),
        Style::default().fg(colour),
    ));

    if !apps_lost.is_empty() {
        // Naming applications rather than packages is the whole point of the
        // preview: "this will also remove GIMP" beats "this will remove gegl".
        lines.push(Line::styled(
            format!("applications lost: {}", apps_lost.join(", ")),
            Style::default().fg(DANGER),
        ));
    }
    if !plan.optdep_losses.is_empty() {
        let names: Vec<&str> = plan
            .optdep_losses
            .iter()
            .map(|(dependent, _)| ui.state.graph.name(*dependent))
            .take(5)
            .collect();
        lines.push(Line::styled(
            format!("would silently degrade: {}", names.join(", ")),
            Style::default().fg(WARN),
        ));
    }

    lines
}

fn draw_keybar(f: &mut Frame, area: Rect, ui: &Ui) {
    let hints: Vec<(&str, &str)> = if ui.dialog.is_some() {
        vec![("↑↓", "mode"), ("Enter", "confirm"), ("Ctrl+S", "snapshot"), ("Esc", "cancel")]
    } else if ui.searching {
        vec![("Esc", "cancel"), ("Enter", "keep"), ("↑↓", "move")]
    } else {
        let mut h = vec![
            ("F1", "help"),
            ("1-4", "views"),
            ("Ctrl+F", "search"),
            ("→", "open"),
            ("←", "back"),
            ("Del", "remove"),
            ("Ctrl+Z", "undo"),
        ];
        if ui.view == View::Orphans {
            h.push(("c", "clean all"));
            h.push((
                "Space",
                match ui.orphan_mode {
                    OrphanMode::Conservative => "-Qdt (safer)",
                    OrphanMode::Aggressive => "-Qdtt (wider)",
                },
            ));
        }
        h.push(("Ctrl+Q", "quit"));
        h
    };

    // A transient message replaces the hints: it is why the last keypress did
    // nothing, and that is more useful than the hints for one moment.
    if let Some(notice) = &ui.notice {
        f.render_widget(
            Paragraph::new(Line::styled(
                format!(" {notice} "),
                Style::default().fg(Color::Black).bg(WARN),
            )),
            area,
        );
        return;
    }

    let mut spans: Vec<Span> = Vec::new();
    for (key, what) in hints {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default().fg(Color::Black).bg(ACCENT),
        ));
        spans.push(Span::styled(format!(" {what}  "), Style::default().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help(f: &mut Frame, area: Rect, ui: &Ui) {
    let w = area.width.min(66);
    let h = area.height.min(22);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };

    let mut lines = vec![
        Line::styled("apothiki — read-only explorer", Style::default().fg(ACCENT)),
        Line::raw(""),
        Line::raw("1 2 3 4        Apps / Tools / Dependencies / Orphans"),
        Line::raw("↑ ↓ PgUp PgDn Home End   move"),
        Line::raw("Ctrl+F         search        F5   refresh"),
        Line::raw("→ or Enter     open: list → relationships → package"),
        Line::raw("← or Backspace go back"),
        Line::raw("Del            remove the selected package"),
        Line::raw("Ctrl+Z         undo the last removal (from package cache)"),
        Line::raw("c  (Orphans)   clean up all orphans"),
        Line::raw("Space (Orphans) toggle -Qdt / -Qdtt"),
        Line::raw("Ctrl+Q         quit"),
        Line::raw(""),
        Line::styled(
            "Removals run through pacman itself, never by writing",
            Style::default().fg(OK),
        ),
        Line::styled(
            "to its database. Every plan is checked against a",
            Style::default().fg(OK),
        ),
        Line::styled("pacman dry-run before it runs.", Style::default().fg(OK)),
        Line::raw(""),
    ];
    if !ui.enhanced_keys {
        lines.push(Line::styled(
            "Terminal lacks the Kitty keyboard protocol;",
            Style::default().fg(DIM),
        ));
        lines.push(Line::styled(
            "fallback bindings are in use.",
            Style::default().fg(DIM),
        ));
    }
    lines.push(Line::styled("press any key", Style::default().fg(DIM)));

    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(" help "),
        ),
        popup,
    );
}

fn field<'a>(name: &'a str, value: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{name:<15}"), Style::default().fg(DIM)),
        Span::raw(value.to_string()),
    ])
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Formats a unix timestamp as `YYYY-MM-DD`.
///
/// Hand-rolled civil-from-days rather than pulling in `chrono` for one format:
/// the binary size target is 5 MB and this is the only date in the product.
fn format_date(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1_580_620), "1.5 MiB");
        assert_eq!(human_size(21 * 1024 * 1024 * 1024), "21.0 GiB");
    }

    #[test]
    fn dates_match_known_timestamps() {
        assert_eq!(format_date(0), "1970-01-01");
        // Cross-checked against `date -u -d @1783277044`.
        assert_eq!(format_date(1_783_277_044), "2026-07-05");
        // Leap day.
        assert_eq!(format_date(1_709_164_800), "2024-02-29");
    }
}
