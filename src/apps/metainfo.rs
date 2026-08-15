//! Layer 1: local AppStream metainfo (spec §4.2).
//!
//! The strongest evidence available. These files were installed *by a package*,
//! so the owning package is known exactly from the reverse file index — no
//! guessing — and the upstream author wrote the name and summary themselves.
//!
//! Measured on the dev machine: 95 files, of which only **45 carry a
//! `<launchable>` tag**, so the bridge to Layer 2 cannot rely on it alone.
//! Component types found in the wild are `desktop-application` (32), the legacy
//! alias `desktop` (21), `addon` (21), `font` (12), `console-application` (5)
//! and `generic` (1) — the type table below is not hypothetical.

use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    /// A launchable GUI application. `desktop` is the legacy spelling of
    /// `desktop-application`; both mean the same thing.
    DesktopApplication,
    /// A CLI program shipping metainfo. Belongs in Tools, not Apps.
    ConsoleApplication,
    /// Belongs *to* another component via `<extends>`. Never a top-level app.
    Addon,
    /// Fonts, codecs, drivers, generic components — not applications.
    Other,
}

impl ComponentKind {
    fn parse(s: &str) -> Self {
        match s {
            "desktop-application" | "desktop" => ComponentKind::DesktopApplication,
            "console-application" => ComponentKind::ConsoleApplication,
            "addon" => ComponentKind::Addon,
            _ => ComponentKind::Other,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Component {
    /// Reverse-DNS identity, e.g. `org.gnome.Meld`. Legacy files use the
    /// desktop file id here instead, e.g. `org.kde.dolphin.desktop`.
    pub id: String,
    pub kind: ComponentKind,
    pub name: Option<String>,
    pub summary: Option<String>,
    /// `<launchable type="desktop-id">` — the official bridge to Layer 2.
    pub launchable: Option<String>,
    pub categories: Vec<String>,
    /// `<extends>` — the component this addon belongs to.
    pub extends: Vec<String>,
    pub path: PathBuf,
}

impl Component {
    /// The desktop file id this component should join to, if any.
    ///
    /// Prefers the explicit `<launchable>`, then falls back to the id itself
    /// when it already names a desktop file (the legacy convention), and
    /// finally tries appending `.desktop`. Without the fallbacks, more than half
    /// of the local components would fail to join.
    pub fn desktop_id(&self) -> Option<String> {
        if let Some(l) = &self.launchable {
            return Some(l.clone());
        }
        if self.kind != ComponentKind::DesktopApplication {
            return None;
        }
        if self.id.ends_with(".desktop") {
            return Some(self.id.clone());
        }
        Some(format!("{}.desktop", self.id))
    }
}

/// Preferred locale tags, best first, in AppStream's hyphenated form
/// (`ca-valencia`, not `ca@valencia`).
fn locale_candidates() -> Vec<String> {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .unwrap_or_default();
    let base = raw.split('.').next().unwrap_or("").trim().replace('_', "-");
    if base.is_empty() || base == "C" || base == "POSIX" {
        return Vec::new();
    }
    let mut out = vec![base.clone()];
    if let Some((lang, _)) = base.split_once('-') {
        out.push(lang.to_string());
    }
    out
}

/// Picks the best available translation.
///
/// An entry with no `xml:lang` is the untranslated original and is the fallback;
/// a matching locale wins over it.
fn best<'a>(candidates: &'a [(Option<String>, String)], locales: &[String]) -> Option<&'a str> {
    for loc in locales {
        if let Some((_, v)) = candidates
            .iter()
            .find(|(lang, _)| lang.as_deref() == Some(loc.as_str()))
        {
            return Some(v);
        }
    }
    candidates
        .iter()
        .find(|(lang, _)| lang.is_none())
        .map(|(_, v)| v.as_str())
}

/// Parses one metainfo document. A file may hold a single `<component>` or a
/// `<components>` collection, so both are handled.
pub fn parse(xml: &str, path: &Path) -> Vec<Component> {
    let locales = locale_candidates();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut depth = 0usize;
    // Depth at which the current <component> sits, so nested elements such as
    // <developer><name> are never mistaken for the component's own name.
    let mut component_depth: Option<usize> = None;

    let mut kind = ComponentKind::Other;
    let mut id: Option<String> = None;
    let mut names: Vec<(Option<String>, String)> = Vec::new();
    let mut summaries: Vec<(Option<String>, String)> = Vec::new();
    let mut launchable: Option<String> = None;
    let mut categories: Vec<String> = Vec::new();
    let mut extends: Vec<String> = Vec::new();

    // What the next text event belongs to, and its xml:lang.
    let mut capture: Option<(&'static str, Option<String>)> = None;

    let attr = |e: &quick_xml::events::BytesStart, want: &[u8]| -> Option<String> {
        e.attributes().flatten().find_map(|a| {
            (a.key.as_ref() == want)
                .then(|| String::from_utf8_lossy(a.value.as_ref()).into_owned())
        })
    };

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                depth += 1;

                if tag == "component" {
                    component_depth = Some(depth);
                    kind = attr(&e, b"type")
                        .map(|t| ComponentKind::parse(&t))
                        .unwrap_or(ComponentKind::Other);
                    id = None;
                    names.clear();
                    summaries.clear();
                    launchable = None;
                    categories.clear();
                    extends.clear();
                    continue;
                }

                let Some(cd) = component_depth else { continue };
                let lang = attr(&e, b"xml:lang");

                if depth == cd + 1 {
                    capture = match tag.as_str() {
                        "id" => Some(("id", None)),
                        "name" => Some(("name", lang)),
                        "summary" => Some(("summary", lang)),
                        "extends" => Some(("extends", None)),
                        "launchable" => {
                            // Only desktop-id launchables bridge to Layer 2;
                            // `service` and `url` types do not.
                            (attr(&e, b"type").as_deref() == Some("desktop-id"))
                                .then_some(("launchable", None))
                        }
                        _ => None,
                    };
                } else if depth == cd + 2 && tag == "category" {
                    capture = Some(("category", None));
                }
            }
            Ok(Event::Text(t)) => {
                if let Some((what, lang)) = capture.take() {
                    // Decodes and resolves entities (`&amp;` → `&`).
                    let text = t.xml10_content().unwrap_or_default().trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    match what {
                        "id" => id = Some(text),
                        "name" => names.push((lang, text)),
                        "summary" => summaries.push((lang, text)),
                        "launchable" => launchable = Some(text),
                        "category" => categories.push(text),
                        "extends" => extends.push(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                capture = None;
                if tag == "component" {
                    if let Some(id) = id.take() {
                        out.push(Component {
                            id,
                            kind,
                            name: best(&names, &locales).map(|s| s.to_string()),
                            summary: best(&summaries, &locales).map(|s| s.to_string()),
                            launchable: launchable.take(),
                            categories: std::mem::take(&mut categories),
                            extends: std::mem::take(&mut extends),
                            path: path.to_path_buf(),
                        });
                    }
                    component_depth = None;
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Empty(e)) => {
                // Self-closing launchable/category tags carry no text.
                let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if tag == "component" {
                    component_depth = None;
                }
            }
            Ok(Event::Eof) => break,
            // A malformed file must not abort the scan of the other 94.
            Err(_) => break,
            _ => {}
        }
    }

    out
}

/// Standard locations for locally installed metainfo, newest convention first.
pub fn search_dirs() -> Vec<PathBuf> {
    ["/usr/share/metainfo", "/usr/share/appdata"]
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}

/// Reads and parses every metainfo file in `dirs`.
pub fn scan(dirs: &[PathBuf]) -> Vec<Component> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "xml") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                out.extend(parse(&text, &path));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_modern_component() {
        let xml = r#"<?xml version="1.0"?>
            <component type="desktop-application">
              <id>org.gnome.Meld</id>
              <name>Meld</name>
              <summary>Compare and merge your files</summary>
              <launchable type="desktop-id">org.gnome.Meld.desktop</launchable>
              <categories><category>Development</category><category>Utility</category></categories>
            </component>"#;
        let c = &parse(xml, Path::new("/x.xml"))[0];
        assert_eq!(c.id, "org.gnome.Meld");
        assert_eq!(c.kind, ComponentKind::DesktopApplication);
        assert_eq!(c.name.as_deref(), Some("Meld"));
        assert_eq!(c.summary.as_deref(), Some("Compare and merge your files"));
        assert_eq!(c.desktop_id().as_deref(), Some("org.gnome.Meld.desktop"));
        assert_eq!(c.categories, ["Development", "Utility"]);
    }

    #[test]
    fn legacy_desktop_type_and_missing_launchable_still_join() {
        // 21 of 95 files on the dev machine use type="desktop", and only 45
        // carry a <launchable> at all. Without these fallbacks most components
        // would never bind to their desktop entry.
        let xml = r#"<component type="desktop">
              <id>org.kde.dolphin.desktop</id>
              <name>Dolphin</name>
            </component>"#;
        let c = &parse(xml, Path::new("/x.xml"))[0];
        assert_eq!(c.kind, ComponentKind::DesktopApplication);
        assert_eq!(c.desktop_id().as_deref(), Some("org.kde.dolphin.desktop"));

        let xml2 = r#"<component type="desktop-application">
              <id>org.example.Thing</id><name>Thing</name>
            </component>"#;
        let c2 = &parse(xml2, Path::new("/x.xml"))[0];
        assert_eq!(c2.desktop_id().as_deref(), Some("org.example.Thing.desktop"));
    }

    #[test]
    fn nested_name_does_not_shadow_the_component_name() {
        // <developer><name>KDE</name></developer> appears before <name> in real
        // KDE metainfo and would otherwise win.
        let xml = r#"<component type="desktop-application">
              <id>org.kde.dolphin</id>
              <developer id="org.kde"><name>KDE</name><url>https://kde.org</url></developer>
              <name>Dolphin</name>
            </component>"#;
        let c = &parse(xml, Path::new("/x.xml"))[0];
        assert_eq!(c.name.as_deref(), Some("Dolphin"));
    }

    #[test]
    fn addons_and_fonts_are_not_launchable() {
        for (ty, kind) in [
            ("addon", ComponentKind::Addon),
            ("font", ComponentKind::Other),
            ("console-application", ComponentKind::ConsoleApplication),
        ] {
            let xml = format!(
                r#"<component type="{ty}"><id>x.y</id><name>N</name>
                   <extends>org.parent.App</extends></component>"#
            );
            let c = &parse(&xml, Path::new("/x.xml"))[0];
            assert_eq!(c.kind, kind);
            assert_eq!(c.desktop_id(), None, "{ty} must not claim a desktop id");
        }
    }

    #[test]
    fn untranslated_name_wins_when_no_locale_matches() {
        let xml = r#"<component type="desktop-application">
              <id>x.y</id>
              <name>Dolphin</name>
              <name xml:lang="ar">دولفين</name>
              <name xml:lang="zz">Nope</name>
            </component>"#;
        let c = &parse(xml, Path::new("/x.xml"))[0];
        // Whatever the test machine's locale, it is not `zz`, and the
        // untranslated entry must be the fallback rather than the last one seen.
        assert!(matches!(c.name.as_deref(), Some("Dolphin") | Some("دولفين")));
    }

    #[test]
    fn a_collection_document_yields_every_component() {
        let xml = r#"<components version="0.14">
              <component type="desktop-application"><id>a.b</id><name>A</name></component>
              <component type="desktop-application"><id>c.d</id><name>C</name></component>
            </components>"#;
        let cs = parse(xml, Path::new("/x.xml"));
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[1].name.as_deref(), Some("C"));
    }

    #[test]
    fn malformed_xml_does_not_panic() {
        parse("<component type=\"desktop\"><id>broken", Path::new("/x.xml"));
        parse("", Path::new("/x.xml"));
        parse("not xml at all", Path::new("/x.xml"));
    }
}
