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

/// The palette, set once at startup from the user's config.
///
/// A global rather than a parameter because roughly a hundred call sites need
/// it, half of them in helpers that have no reason to know about `Ui`. It is
/// written once before the first frame and only read afterwards.
static THEME: std::sync::OnceLock<crate::config::Theme> = std::sync::OnceLock::new();

/// Installs the palette. Called before the first draw; later calls are ignored.
pub fn set_theme(theme: crate::config::Theme) {
    let _ = THEME.set(theme);
}

fn theme() -> &'static crate::config::Theme {
    THEME.get_or_init(crate::config::Theme::default)
}

#[allow(non_snake_case)]
fn ACCENT() -> Color {
    theme().accent
}
#[allow(non_snake_case)]
fn DIM() -> Color {
    theme().dim
}
#[allow(non_snake_case)]
fn WARN() -> Color {
    theme().warn
}
#[allow(non_snake_case)]
fn DANGER() -> Color {
    theme().danger
}
#[allow(non_snake_case)]
fn OK() -> Color {
    theme().ok
}

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
    if ui.locations.is_some() {
        draw_locations(f, f.area(), ui);
    }
    if ui.dialog.is_some() {
        draw_removal(f, f.area(), ui);
    }
}

/// Where a package's files live (spec §14).
///
/// Facts and guesses are kept visually apart. What the package owns comes from
/// pacman; what lives in the user's home is matched by name and could easily be
/// wrong, so it is labelled rather than presented alongside as equal.
fn draw_locations(f: &mut Frame, area: Rect, ui: &Ui) {
    let Some((name, groups)) = &ui.locations else {
        return;
    };
    let popup = centred(area, area.width.saturating_sub(8), area.height.saturating_sub(4));

    let mut lines: Vec<Line> = Vec::new();
    // Selectable rows are counted in the same order `location_paths` produces,
    // so the highlight and the Enter target cannot drift apart.
    let mut selectable = 0usize;

    for group in groups {
        lines.push(Line::styled(
            group.title.to_string(),
            Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
        ));
        lines.push(Line::styled(
            group.explanation.to_string(),
            Style::default().fg(DIM()),
        ));
        for entry in &group.paths {
            let is_selected = entry.exists && selectable == ui.locations_selected;
            if entry.exists {
                selectable += 1;
            }
            let marker = if is_selected { "▌ " } else { "  " };
            let mut spans = vec![Span::styled(
                format!("{marker}{}", entry.path),
                Style::default()
                    .fg(if entry.guessed { WARN() } else { Color::Reset })
                    .add_modifier(if is_selected {
                        Modifier::BOLD | Modifier::REVERSED
                    } else {
                        Modifier::empty()
                    }),
            )];
            if let Some(size) = entry.size {
                spans.push(Span::styled(
                    format!("  {}", human_size(size)),
                    Style::default().fg(DIM()),
                ));
            }
            if entry.guessed {
                spans.push(Span::styled("  (guess)", Style::default().fg(DIM())));
            }
            if !entry.exists {
                spans.push(Span::styled("  (not present)", Style::default().fg(DIM())));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));
    }

    if groups.is_empty() {
        lines.push(Line::styled(
            "This package owns no files worth listing.",
            Style::default().fg(DIM()),
        ));
    }

    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines)
            .scroll((ui.locations_scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ACCENT()))
                    .title(format!(
                        " files of {name} — ↑↓ select, → open, PgUp/PgDn scroll, Esc close "
                    )),
            ),
        popup,
    );
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
            if let Some(plan) = d.job.as_update() {
                draw_update_confirm(f, popup, plan, d.snapshot);
                return;
            }
            if let Some(r) = d.job.as_flatpak() {
                draw_flatpak_confirm(f, popup, r);
                return;
            }
            if let Some(r) = d.job.as_appimage() {
                draw_appimage_confirm(f, popup, r);
                return;
            }
            if let Some(u) = d.job.as_single_update() {
                draw_single_update_confirm(f, popup, u, d.snapshot);
                return;
            }
            if let Some(request) = d.job.as_install() {
                if let Some(text) = &d.pkgbuild {
                    draw_pkgbuild(f, area, request, text, d.pkgbuild_scroll);
                } else {
                    draw_install_confirm(f, popup, request);
                }
                return;
            }
            let Some(req) = d.job.as_removal() else { return };
            let names = req.package_names(graph);
            lines.push(Line::styled(
                names.join(", "),
                Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
            ));

            // A blocked removal explains itself and offers nothing. There is no
            // override, by design (spec §6.1).
            if let Some(p) = &req.blocked_by {
                lines.push(Line::raw(""));
                lines.push(Line::styled(p.explain(), Style::default().fg(DANGER())));
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "There is no way to force this, and that is deliberate.",
                    Style::default().fg(DIM()),
                ));
                lines.push(Line::styled("Esc to close", Style::default().fg(DIM())));
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
                    Style::default().fg(DANGER()),
                ));
                lines.push(Line::styled(
                    "pacman would refuse this removal.",
                    Style::default().fg(DIM()),
                ));
                lines.push(Line::styled("Esc to close", Style::default().fg(DIM())));
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
                        Style::default().fg(DIM())
                    },
                ));
                if selected {
                    lines.push(Line::styled(
                        format!("    {}", m.detail()),
                        Style::default().fg(DIM()),
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
                    Style::default().fg(DANGER()),
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
                    Style::default().fg(WARN()),
                ));
            }

            let risk_colour = match req.risk {
                Risk::Safe => OK(),
                Risk::Caution => WARN(),
                _ => DANGER(),
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
                Style::default().fg(if d.snapshot { OK() } else { DIM() }),
            ));
            lines.push(Line::styled(
                format!("$ {}", req.command_line(graph)),
                Style::default().fg(DIM()),
            ));

            if matches!(d.stage, Stage::TypeToConfirm) {
                use super::removal::ConfirmState;
                let state = d.confirmation_state();
                let colour = match state {
                    ConfirmState::Matches => OK(),
                    ConfirmState::Wrong => DANGER(),
                    _ => WARN(),
                };
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!("Type \"{}\" to confirm:", d.confirm_word),
                    Style::default().fg(DANGER()).add_modifier(Modifier::BOLD),
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
                Style::default().fg(DIM()),
            ));
        }
        Stage::Password => {
            lines.push(Line::raw("Administrator password required."));
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!("  {}▏", "•".repeat(d.password.chars().count()))));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Used only to authorise this operation, then discarded.",
                Style::default().fg(DIM()),
            ));
            if let Some(e) = &d.error {
                lines.push(Line::styled(e.clone(), Style::default().fg(DANGER())));
            }
            lines.push(Line::styled("Enter confirm   Esc cancel", Style::default().fg(DIM())));
        }
        Stage::Running | Stage::Done { .. } => {
            for l in d.output.iter().rev().take(20).collect::<Vec<_>>().into_iter().rev() {
                lines.push(Line::raw(l.clone()));
            }
            if let Some(e) = &d.error {
                lines.push(Line::styled(e.clone(), Style::default().fg(DANGER())));
            }
            match d.stage {
                Stage::Done { success: true } => {
                    lines.push(Line::styled("Finished.", Style::default().fg(OK())));
                    lines.push(Line::styled("Esc to close", Style::default().fg(DIM())));
                }
                Stage::Done { success: false } => {
                    lines.push(Line::styled(
                        "Nothing was changed.",
                        Style::default().fg(WARN()),
                    ));
                    lines.push(Line::styled("Esc to close", Style::default().fg(DIM())));
                }
                _ => {}
            }
        }
    }

    render_popup(f, popup, title, lines);
}

/// The PKGBUILD review pane.
///
/// Shown nearly full-screen: a PKGBUILD is a shell script that will run with
/// the user's privileges, and reviewing it through a letterbox is not
/// reviewing it.
fn draw_pkgbuild(
    f: &mut Frame,
    area: Rect,
    request: &crate::ops::InstallRequest,
    text: &str,
    scroll: u16,
) {
    let popup = centred(area, area.width.saturating_sub(6), area.height.saturating_sub(4));
    let lines: Vec<Line> = text.lines().map(|l| Line::raw(l.to_string())).collect();

    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).scroll((scroll, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(WARN()))
                .title(format!(
                    " PKGBUILD for {} — PgUp/PgDn scroll, P back ",
                    request.package
                )),
        ),
        popup,
    );
}

/// Removing a Flatpak.
fn draw_flatpak_confirm(f: &mut Frame, popup: Rect, r: &crate::ops::bundle::FlatpakRemoval) {
    let mut lines: Vec<Line> = vec![Line::styled(
        r.name.clone(),
        Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
    )];
    lines.push(Line::raw(""));
    lines.push(field("flatpak id", &r.id));
    lines.push(field(
        "installed for",
        if r.system {
            "everyone (needs your password)"
        } else {
            "you only"
        },
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!(
            "[{}] also remove unused runtimes",
            if r.remove_unused { "x" } else { " " }
        ),
        Style::default().fg(if r.remove_unused { OK() } else { DIM() }),
    ));
    lines.push(Line::styled(
        "Runtimes are Flatpak's shared dependencies — this is where the",
        Style::default().fg(DIM()),
    ));
    lines.push(Line::styled(
        "space actually comes back.",
        Style::default().fg(DIM()),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("$ {}", r.command_line()),
        Style::default().fg(DIM()),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Enter remove   Esc cancel".to_string(),
        Style::default().fg(DIM()),
    ));
    render_popup(f, popup, " remove flatpak ", lines);
}

/// Removing an AppImage, component by component.
fn draw_appimage_confirm(f: &mut Frame, popup: Rect, r: &crate::ops::bundle::AppImageRemoval) {
    let mut lines: Vec<Line> = vec![Line::styled(
        r.name.clone(),
        Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
    )];
    lines.push(Line::styled(
        "A self-contained bundle — nothing else depends on it.",
        Style::default().fg(DIM()),
    ));
    lines.push(Line::raw(""));

    lines.push(Line::styled(
        format!("[x] {}", r.bundle.display()),
        Style::default().fg(OK()),
    ));
    lines.push(Line::styled(
        "    the application itself",
        Style::default().fg(DIM()),
    ));

    let mut row = |on: bool, key: char, path: Option<&std::path::PathBuf>, what: &str| {
        if let Some(p) = path {
            lines.push(Line::styled(
                format!("[{}] {}   ({key})", if on { "x" } else { " " }, p.display()),
                Style::default().fg(if on { OK() } else { DIM() }),
            ));
            lines.push(Line::styled(format!("    {what}"), Style::default().fg(DIM())));
        }
    };
    row(r.remove_desktop, '1', r.desktop_entry.as_ref(), "its launcher entry");
    row(r.remove_icon, '2', r.icon.as_ref(), "its icon");

    if !r.user_data.is_empty() {
        let first = r.user_data.first();
        lines.push(Line::styled(
            format!(
                "[{}] {}   (3)",
                if r.remove_data { "x" } else { " " },
                first.map(|p| p.display().to_string()).unwrap_or_default()
            ),
            Style::default().fg(if r.remove_data { DANGER() } else { DIM() }),
        ));
        lines.push(Line::styled(
            "    your settings and data — matched by name, so this is a guess.",
            Style::default().fg(WARN()),
        ));
        lines.push(Line::styled(
            "    Off by default; nothing else here is uncertain.",
            Style::default().fg(DIM()),
        ));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("{} item(s) will be deleted.", r.targets().len()),
        Style::default().fg(WARN()),
    ));
    lines.push(Line::styled(
        "This is a plain file deletion — there is no undo and no snapshot.",
        Style::default().fg(DANGER()),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "1/2/3 toggle   Enter remove   Esc cancel".to_string(),
        Style::default().fg(DIM()),
    ));
    render_popup(f, popup, " remove appimage ", lines);
}

/// Upgrading one package.
///
/// The risk is stated first and in full, because this is the path where a
/// reasonable-looking action breaks the system days later, in a way that will
/// not obviously connect back to this keypress.
fn draw_single_update_confirm(
    f: &mut Frame,
    popup: Rect,
    u: &crate::ops::update::Update,
    snapshot: bool,
) {
    use crate::ops::update::UpdateSource;

    let mut lines: Vec<Line> = vec![Line::styled(
        format!("{}  {} → {}", u.name, u.installed, u.available),
        Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
    )];
    lines.push(Line::raw(""));

    if u.source == UpdateSource::Repo {
        lines.push(Line::styled(
            "This is a partial upgrade.",
            Style::default().fg(DANGER()).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            "The new build expects library versions the rest of your system",
            Style::default().fg(DIM()),
        ));
        lines.push(Line::styled(
            "does not have yet. It may work, or it may break this program —",
            Style::default().fg(DIM()),
        ));
        lines.push(Line::styled(
            "or your session — in ways that surface days later.",
            Style::default().fg(DIM()),
        ));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Esc, then u, upgrades everything together instead.",
            Style::default().fg(OK()),
        ));
    } else {
        lines.push(Line::styled(
            "Rebuilt from source by the AUR helper.",
            Style::default().fg(WARN()),
        ));
        lines.push(Line::styled(
            "Safer than a partial repository upgrade, but the build can still",
            Style::default().fg(DIM()),
        ));
        lines.push(Line::styled(
            "pull repository packages forward on its own.",
            Style::default().fg(DIM()),
        ));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!(
            "[{}] snapper snapshot first  (Ctrl+S)",
            if snapshot { "x" } else { " " }
        ),
        Style::default().fg(if snapshot { OK() } else { DIM() }),
    ));
    lines.push(Line::styled(
        format!(
            "$ {} -S {}",
            match u.source {
                UpdateSource::Repo => "sudo pacman",
                UpdateSource::Aur => "paru",
            },
            u.name
        ),
        Style::default().fg(DIM()),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Enter upgrade anyway   Esc cancel".to_string(),
        Style::default().fg(DIM()),
    ));
    render_popup(f, popup, " upgrade one package ", lines);
}

/// The update dialog.
///
/// Says plainly that the whole system is upgraded, not the one package the user
/// was looking at. That is not a limitation to apologise for: upgrading a subset
/// of a rolling release is what breaks it.
fn draw_update_confirm(
    f: &mut Frame,
    popup: Rect,
    plan: &crate::ops::update::UpdatePlan,
    snapshot: bool,
) {
    let mut lines: Vec<Line> = vec![Line::styled(
        format!("{} update(s) available", plan.total()),
        Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
    )];
    lines.push(Line::raw(""));

    if !plan.repo.is_empty() {
        lines.push(Line::styled(
            format!("Repository ({})", plan.repo.len()),
            Style::default().fg(OK()),
        ));
        for u in plan.repo.iter().take(8) {
            lines.push(Line::raw(format!(
                "  {}  {} → {}",
                u.name, u.installed, u.available
            )));
        }
        if plan.repo.len() > 8 {
            lines.push(Line::styled(
                format!("  … and {} more", plan.repo.len() - 8),
                Style::default().fg(DIM()),
            ));
        }
    }
    if !plan.aur.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("AUR ({}) — rebuilt from source", plan.aur.len()),
            Style::default().fg(WARN()),
        ));
        for u in plan.aur.iter().take(6) {
            lines.push(Line::raw(format!(
                "  {}  {} → {}",
                u.name, u.installed, u.available
            )));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "The whole system is upgraded together.",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::styled(
        "Upgrading one package on its own is a partial upgrade: it links",
        Style::default().fg(DIM()),
    ));
    lines.push(Line::styled(
        "against libraries the rest of the system does not have yet, and is",
        Style::default().fg(DIM()),
    ));
    lines.push(Line::styled(
        "the most common way a rolling install gets broken.",
        Style::default().fg(DIM()),
    ));

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!(
            "[{}] snapper snapshot first  (Ctrl+S)",
            if snapshot { "x" } else { " " }
        ),
        Style::default().fg(if snapshot { OK() } else { DIM() }),
    ));
    lines.push(Line::styled(
        "$ sudo pacman -Syu".to_string(),
        Style::default().fg(DIM()),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Enter upgrade   Esc cancel".to_string(),
        Style::default().fg(DIM()),
    ));
    render_popup(f, popup, " update ", lines);
}

/// The install dialog.
///
/// States the source plainly. The difference between a signed repository build
/// and a user-submitted PKGBUILD that compiles on your machine is the single
/// most important thing a newcomer to Arch does not know.
fn draw_install_confirm(f: &mut Frame, popup: Rect, request: &crate::ops::InstallRequest) {
    let mut lines: Vec<Line> = vec![Line::styled(
        format!("{} {}", request.package, request.version),
        Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
    )];
    lines.push(Line::raw(""));
    lines.push(field(
        "source",
        match request.source {
            crate::ops::InstallSource::Repo => "repository (signed, reviewed)",
            crate::ops::InstallSource::Aur => "AUR (built from source)",
        },
    ));
    if let Some(h) = &request.helper {
        lines.push(field("helper", h));
    }

    if !request.warnings.is_empty() {
        lines.push(Line::raw(""));
        for w in &request.warnings {
            lines.push(Line::styled(w.clone(), Style::default().fg(WARN())));
        }
    }

    if request.source == crate::ops::InstallSource::Aur {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Press P to read the PKGBUILD before installing.",
            Style::default().fg(ACCENT()),
        ));
        lines.push(Line::styled(
            "It is a shell script that runs on your machine.",
            Style::default().fg(DIM()),
        ));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("$ {}", request.command_line()),
        Style::default().fg(DIM()),
    ));
    lines.push(Line::raw(""));

    let blocked = request.source == crate::ops::InstallSource::Aur && request.helper.is_none();
    lines.push(Line::styled(
        if blocked {
            "Cannot continue without an AUR helper.   Esc to close"
        } else {
            "Enter install   Esc cancel"
        }
        .to_string(),
        Style::default().fg(if blocked { DANGER() } else { DIM() }),
    ));
    render_popup(f, popup, " install ", lines);
}

/// The undo dialog.
///
/// States plainly what can and cannot be brought back: a purge deleted config
/// files, and no reinstall returns them.
fn draw_restore_confirm(f: &mut Frame, popup: Rect, plan: &crate::ops::restore::RestorePlan) {
    let mut lines: Vec<Line> = vec![Line::styled(
        "Undo the last removal",
        Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
    )];
    lines.push(Line::styled(
        format!("removed with {} on {}", plan.operation, format_date(plan.timestamp)),
        Style::default().fg(DIM()),
    ));
    lines.push(Line::raw(""));

    if !plan.missing.is_empty() {
        lines.push(Line::styled(
            format!(
                "{} package(s) are no longer in the package cache:",
                plan.missing.len()
            ),
            Style::default().fg(DANGER()),
        ));
        for (n, v) in plan.missing.iter().take(6) {
            lines.push(Line::styled(format!("  {n} {v}"), Style::default().fg(DIM())));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "A partial restore would leave the system in a state neither you",
            Style::default().fg(DIM()),
        ));
        lines.push(Line::styled(
            "nor this tool could describe, so it is not offered.",
            Style::default().fg(DIM()),
        ));
        lines.push(Line::styled(
            "Reinstall from the repositories instead, or roll back the snapshot.",
            Style::default().fg(DIM()),
        ));
        lines.push(Line::styled("Esc to close", Style::default().fg(DIM())));
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
            Style::default().fg(DIM()),
        ));
    }

    if plan.configs_were_purged {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "That removal was a purge: config files were deleted and cannot",
            Style::default().fg(WARN()),
        ));
        lines.push(Line::styled(
            "be restored by reinstalling. Only the packages come back.",
            Style::default().fg(WARN()),
        ));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("$ {}", plan.command_line()),
        Style::default().fg(DIM()),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Enter restore   Esc cancel",
        Style::default().fg(DIM()),
    ));
    render_popup(f, popup, " undo ", lines);
}

fn render_popup(f: &mut Frame, popup: Rect, title: &str, lines: Vec<Line>) {
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DANGER()))
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
                View::Search => ui.results.len(),
                View::Updates => ui.updates.total(),
            };
            Line::from(format!(" {} ({count}) ", v.title()))
        })
        .collect();

    let selected = View::ALL.iter().position(|v| *v == ui.view).unwrap_or(0);
    if !ui.updates.is_empty() {
        let label = format!(" {} updates (u) ", ui.updates.total());
        let w = label.chars().count() as u16;
        f.render_widget(
            Paragraph::new(Line::styled(
                label,
                Style::default().fg(Color::Black).bg(OK()),
            )),
            Rect {
                x: area.x + area.width.saturating_sub(w + 14),
                width: w.min(area.width),
                ..area
            },
        );
    }
    if ui.is_reloading() {
        // A refresh rescans the whole system; saying so beats a list that
        // appears frozen.
        let w = area.width.saturating_sub(14);
        f.render_widget(
            Paragraph::new(Line::styled(" refreshing… ", Style::default().fg(OK()))),
            Rect { x: area.x + w, width: 13.min(area.width), ..area },
        );
    }
    f.render_widget(
        Tabs::new(titles)
            .select(selected)
            .style(Style::default().fg(DIM()))
            .highlight_style(Style::default().fg(ACCENT()).add_modifier(Modifier::BOLD))
            .divider(""),
        area,
    );
}

/// A concurrent pacman transaction must be visible, not discovered through a
/// confusing failure later (spec §5.1).
fn draw_lock_banner(f: &mut Frame, area: Rect) {
    f.render_widget(
        Paragraph::new(" another pacman process is running — data may be out of date ")
            .style(Style::default().fg(Color::Black).bg(WARN())),
        area,
    );
}

fn draw_list(f: &mut Frame, area: Rect, ui: &mut Ui) {
    let focused = ui.focus == Focus::List;
    let title = if ui.view == View::Search {
        let state = match ui.aur_state {
            crate::data::aur::AurState::Downloading => "  (AUR index downloading…)",
            crate::data::aur::AurState::Failed => "  (AUR unavailable)",
            _ => "",
        };
        // The cursor only appears when the field actually has focus, so that
        // "Escape released it" is visible rather than something to remember.
        let cursor = if ui.searching { "▏" } else { "" };
        let hint = if ui.searching { "" } else { "  Ctrl+F to type" };
        format!(
            " search: {}{cursor} ({} results){state}{hint} ",
            ui.query,
            ui.results.len()
        )
    } else if ui.searching || !ui.query.is_empty() {
        format!(" filter: {}▏ ({} matches) ", ui.query, ui.rows().len())
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
                    .border_style(Style::default().fg(if focused { ACCENT() } else { DIM() }))
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
                Span::styled(size_suffix(ui, app), Style::default().fg(DIM())),
            ])
        }
        Item::Tool(i) => {
            let tool = &ui.state.catalog.tools[i];
            Line::from(vec![
                Span::raw(tool.name.clone()),
                Span::styled(size_suffix(ui, tool), Style::default().fg(DIM())),
            ])
        }
        Item::Package(p) => {
            let pkg = &ui.state.db.packages[p as usize];
            Line::from(vec![
                Span::raw(pkg.name.clone()),
                Span::styled(
                    format!("  {}", human_size(pkg.size_bytes())),
                    Style::default().fg(DIM()),
                ),
            ])
        }
        Item::Update(i) => {
            let Some(u) = ui.sorted_updates.get(i) else {
                return Line::raw("");
            };
            let label = u.display_name.clone().unwrap_or_else(|| u.name.clone());
            Line::from(vec![
                Span::styled(
                    format!("{:<8} ", u.kind.label()),
                    Style::default().fg(match u.kind {
                        crate::ops::update::Kind::App => ACCENT(),
                        crate::ops::update::Kind::Tool => Color::Reset,
                        crate::ops::update::Kind::Package => DIM(),
                    }),
                ),
                Span::raw(label),
                Span::styled(
                    format!("  {} → {}", u.installed, u.available),
                    Style::default().fg(DIM()),
                ),
                Span::styled(
                    if u.source == crate::ops::update::UpdateSource::Aur {
                        "  aur"
                    } else {
                        ""
                    },
                    Style::default().fg(WARN()),
                ),
            ])
        }
        Item::Result(i) => {
            let Some(hit) = ui.results.get(i) else {
                return Line::raw("");
            };
            let mut spans = vec![
                Span::styled(
                    format!("{:<5} ", if hit.origin == crate::data::search::Origin::Aur { "aur" } else { "repo" }),
                    Style::default().fg(if hit.origin == crate::data::search::Origin::Aur { WARN() } else { DIM() }),
                ),
                Span::raw(hit.name.clone()),
            ];
            if hit.installed {
                spans.push(Span::styled("  installed", Style::default().fg(OK())));
            }
            // Wording matters here: "out of date" reads as "your system needs
            // an update", which is a different thing entirely and lives in the
            // Updates view. This flag is about the packaging.
            if hit.out_of_date {
                spans.push(Span::styled(
                    "  packaging behind upstream",
                    Style::default().fg(WARN()),
                ));
            }
            if hit.orphaned {
                spans.push(Span::styled("  no maintainer", Style::default().fg(WARN())));
            }
            Line::from(spans)
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
        Source::Steam => ("steam", DIM()),
        Source::Unowned => ("?", WARN()),
    }
}

fn draw_detail(f: &mut Frame, area: Rect, ui: &mut Ui) {
    let mut lines: Vec<Line> = Vec::new();

    match ui.current() {
        None => lines.push(Line::styled("nothing selected", Style::default().fg(DIM()))),
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
        .border_style(Style::default().fg(DIM()))
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
                    .border_style(Style::default().fg(DIM()))
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
                        ("protected — this cannot be removed", DIM())
                    }
                    Some(_) => ("Remove this…  (Del)", DANGER()),
                    None => ("no package to remove", DIM()),
                };
                ListItem::new(Line::styled(
                    text.to_string(),
                    Style::default().fg(colour).add_modifier(Modifier::BOLD),
                ))
            }
            RelatedRow::Relation(r) => {
                let colour = match r.kind {
                    RelationKind::DependsOn => Color::Reset,
                    RelationKind::RequiredBy => ACCENT(),
                    RelationKind::Optional => WARN(),
                };
                let mut spans = vec![
                    Span::styled(format!("{:<12} ", r.kind.label()), Style::default().fg(colour)),
                    Span::raw(ui.state.graph.name(r.pkg).to_string()),
                ];
                if let Some(note) = &r.note {
                    spans.push(Span::styled(format!("  — {note}"), Style::default().fg(DIM())));
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
                    .border_style(Style::default().fg(if related_focused { ACCENT() } else { DIM() }))
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
    if let Item::Result(i) = item {
        if let Some(hit) = ui.results.get(i) {
            detail_hit(hit, lines);
        }
        return;
    }
    if let Item::Update(i) = item {
        if let Some(u) = ui.sorted_updates.get(i) {
            lines.push(Line::styled(
                u.display_name.clone().unwrap_or_else(|| u.name.clone()),
                Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
            ));
            lines.push(Line::raw(""));
            lines.push(field("package", &u.name));
            lines.push(field("installed", &u.installed));
            lines.push(field("available", &u.available));
            lines.push(field(
                "from",
                match u.source {
                    crate::ops::update::UpdateSource::Repo => "repository",
                    crate::ops::update::UpdateSource::Aur => "AUR (rebuilt from source)",
                },
            ));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "u  upgrade the whole system  (recommended)",
                Style::default().fg(OK()),
            ));
            lines.push(Line::styled(
                "→  upgrade only this package",
                Style::default().fg(WARN()),
            ));
        }
    }

    let app = match item {
        Item::App(i) => Some(&ui.state.catalog.apps[i]),
        Item::Tool(i) => Some(&ui.state.catalog.tools[i]),
        _ => None,
    };

    if let Some(app) = app {
        lines.push(Line::styled(
            app.name.clone(),
            Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
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
        lines.push(Line::styled("evidence", Style::default().fg(DIM())));
        for e in &app.evidence {
            lines.push(Line::styled(format!("  {}", evidence_text(e)), Style::default().fg(DIM())));
        }

        if app.source == Source::AppImage {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "self-contained bundle — no dependencies to show",
                Style::default().fg(DIM()),
            ));
        }
        lines.push(Line::raw(""));
    }
}

/// Detail for a search result — a package that may not be installed.
fn detail_hit(hit: &crate::data::search::Hit, lines: &mut Vec<Line>) {
    lines.push(Line::styled(
        hit.name.clone(),
        Style::default().add_modifier(Modifier::BOLD).fg(ACCENT()),
    ));
    if let Some(d) = &hit.description {
        lines.push(Line::raw(d.clone()));
    }
    lines.push(Line::raw(""));
    // When it is installed, the package block below repeats version and
    // description in more detail; repeating them here just pads the pane.
    if !hit.installed {
        lines.push(field("version", &hit.version));
    }
    lines.push(field("source", &hit.source_label()));
    lines.push(field(
        "status",
        if hit.installed { "installed" } else { "not installed" },
    ));

    if hit.origin == crate::data::search::Origin::Aur {
        lines.push(field("votes", &hit.votes.to_string()));
        lines.push(Line::raw(""));
        // AUR packages are user-submitted and build from source. Saying so
        // plainly is the point at which a novice can still decide otherwise.
        lines.push(Line::styled(
            "From the AUR: built from source, not reviewed by Arch.",
            Style::default().fg(WARN()),
        ));
        if hit.orphaned {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "No maintainer.",
                Style::default().fg(WARN()).add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::styled(
                "Nobody has volunteered to look after this AUR entry. It will",
                Style::default().fg(DIM()),
            ));
            lines.push(Line::styled(
                "not be updated, and may stop building as Arch moves on.",
                Style::default().fg(DIM()),
            ));
        }
        if hit.out_of_date {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Packaging is behind upstream.",
                Style::default().fg(WARN()).add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::styled(
                "Users flagged this AUR entry as older than the project's own",
                Style::default().fg(DIM()),
            ));
            lines.push(Line::styled(
                "latest release. This is about the recipe, not about your",
                Style::default().fg(DIM()),
            ));
            lines.push(Line::styled(
                "system — installed updates live in the Updates view.",
                Style::default().fg(DIM()),
            ));
        }
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
        return vec![Line::styled(msg, Style::default().fg(DIM()))];
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
            Style::default().fg(DANGER()),
        ));
        return lines;
    }

    let total = plan.all_removed().len();
    let colour = if total > 20 {
        DANGER()
    } else if total > 5 {
        WARN()
    } else {
        OK()
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
            Style::default().fg(DANGER()),
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
            Style::default().fg(WARN()),
        ));
    }

    lines
}

fn draw_keybar(f: &mut Frame, area: Rect, ui: &Ui) {
    let hints: Vec<(&str, &str)> = if ui.dialog.is_some() {
        vec![("↑↓", "mode"), ("Enter", "confirm"), ("Ctrl+S", "snapshot"), ("Esc", "cancel")]
    } else if ui.searching && ui.view == View::Search {
        vec![
            ("Esc", "stop typing"),
            ("Enter", "install"),
            ("↑↓", "move"),
            ("Tab", "next view"),
        ]
    } else if ui.searching {
        vec![("Esc", "cancel"), ("Enter", "keep"), ("↑↓", "move"), ("Tab", "next view")]
    } else {
        let mut h = vec![
            ("F1", "help"),
            ("1-6/Tab", "views"),
            ("f", "filter"),
            ("→", "open"),
            ("←", "back"),
            ("Del", "remove"),
            ("Ctrl+Z", "undo"),
            ("l", "files"),
            ("q", "quit"),
        ];
        // Upgrading is only offered where the user can see what it would do.
        if ui.view == View::Updates {
            h = vec![
                ("F1", "help"),
                ("1-6/Tab", "views"),
                ("u", "upgrade system"),
                ("→", "upgrade this one"),
                ("l", "files"),
                ("q", "quit"),
            ];
        }
        if ui.view == View::Search {
            h = vec![
                ("F1", "help"),
                ("1-6/Tab", "views"),
                ("f", "type"),
                ("→/Enter", "install"),
                ("q", "quit"),
            ];
        }
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
        h
    };

    // A transient message replaces the hints: it is why the last keypress did
    // nothing, and that is more useful than the hints for one moment.
    if let Some(notice) = &ui.notice {
        f.render_widget(
            Paragraph::new(Line::styled(
                format!(" {notice} "),
                Style::default().fg(Color::Black).bg(WARN()),
            )),
            area,
        );
        return;
    }

    let mut spans: Vec<Span> = Vec::new();
    for (key, what) in hints {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default().fg(Color::Black).bg(ACCENT()),
        ));
        spans.push(Span::styled(format!(" {what}  "), Style::default().fg(DIM())));
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
        Line::styled("apothiki — read-only explorer", Style::default().fg(ACCENT())),
        Line::raw(""),
        Line::raw("1 2 3 4 5 6    Apps / Tools / Deps / Orphans / Search / Updates"),
        Line::raw("Tab / Shift+Tab  next / previous view"),
        Line::raw("↑ ↓ PgUp PgDn Home End   move"),
        Line::raw("f              filter, or edit the search query"),
        Line::raw("Esc            leave the search field, then 1-5 work"),
        Line::raw("F5             refresh"),
        Line::raw("→ or Enter     open: list → relationships → package"),
        Line::raw("← or Backspace go back"),
        Line::raw("Del            remove the selected package"),
        Line::raw("Ctrl+Z         undo the last removal (from package cache)"),
        Line::raw("l              where this package's files live"),
        Line::raw("   in that pane: ↑↓ select, → opens in $EDITOR"),
        Line::raw("u              upgrade the system, when updates exist"),
        Line::raw("c  (Orphans)   clean up all orphans"),
        Line::raw("Space (Orphans) toggle -Qdt / -Qdtt"),
        Line::raw("q              quit"),
        Line::raw(""),
        Line::styled(
            "Ctrl+F and Ctrl+Q also work, and are the only forms",
            Style::default().fg(DIM()),
        ),
        Line::styled("that reach you while typing.", Style::default().fg(DIM())),
        Line::raw(""),
        Line::styled(
            "Removals run through pacman itself, never by writing",
            Style::default().fg(OK()),
        ),
        Line::styled(
            "to its database. Every plan is checked against a",
            Style::default().fg(OK()),
        ),
        Line::styled("pacman dry-run before it runs.", Style::default().fg(OK())),
        Line::raw(""),
    ];
    if !ui.enhanced_keys {
        lines.push(Line::styled(
            "Terminal lacks the Kitty keyboard protocol;",
            Style::default().fg(DIM()),
        ));
        lines.push(Line::styled(
            "fallback bindings are in use.",
            Style::default().fg(DIM()),
        ));
    }
    lines.push(Line::styled("press any key", Style::default().fg(DIM())));

    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT()))
                .title(" help "),
        ),
        popup,
    );
}

fn field<'a>(name: &'a str, value: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{name:<15}"), Style::default().fg(DIM())),
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
