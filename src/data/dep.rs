//! Dependency-string parsing.
//!
//! A dependency is not always a package name. It may be a virtual name resolved
//! through some package's `%PROVIDES%`, and it may carry a version constraint:
//!
//! ```text
//! glibc                 plain name
//! curl>=7.20.0          constrained
//! atk<=2.38.0-2         constrained, upper bound
//! libalpm.so=16-64      soname provide (name is `libalpm.so`)
//! sh                    virtual, provided by bash
//! ```
//!
//! Failing to strip constraints means no graph edge ever matches (spec §13.11),
//! and failing to resolve provides leaves holes that corrupt orphan detection
//! (§13.1). Both bugs are silent, so this parsing is deliberately dull and
//! heavily tested.

/// A dependency reference: a name, plus the constraint text we deliberately do
/// not evaluate.
///
/// Version resolution is unnecessary for an already-consistent installed set —
/// if pacman installed it, the constraints are already satisfied. We keep the
/// raw text only so the UI can show what was declared.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dep {
    /// The bare name, with any constraint stripped. This is the graph key.
    pub name: String,
    /// The constraint as written, e.g. `>=7.20.0`. `None` for a plain name.
    pub constraint: Option<String>,
}

impl Dep {
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        // Package names cannot contain `<`, `>` or `=`, so the first such byte
        // is where the constraint begins. Soname provides (`libfoo.so=1-64`)
        // fall out of the same rule with the name `libfoo.so`.
        match s.find(['<', '>', '=']) {
            Some(i) => Dep {
                name: s[..i].to_string(),
                constraint: Some(s[i..].to_string()),
            },
            None => Dep {
                name: s.to_string(),
                constraint: None,
            },
        }
    }

    /// True for soname-style names such as `libGL.so`, which are provided by a
    /// package rather than being one. Useful for presentation: showing a user
    /// `libGL.so=1-64` in a dependency list is noise, showing `mesa` is not.
    pub fn is_soname(&self) -> bool {
        self.name.contains(".so")
    }
}

impl std::fmt::Display for Dep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)?;
        if let Some(c) = &self.constraint {
            f.write_str(c)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_of(s: &str) -> String {
        Dep::parse(s).name
    }

    #[test]
    fn plain_names_pass_through() {
        let d = Dep::parse("glibc");
        assert_eq!(d.name, "glibc");
        assert_eq!(d.constraint, None);
    }

    #[test]
    fn strips_every_constraint_operator() {
        // All forms observed in a real local db.
        assert_eq!(name_of("curl>=7.20.0"), "curl");
        assert_eq!(name_of("atk<=2.38.0-2"), "atk");
        assert_eq!(name_of("expat=2.8.3"), "expat");
        assert_eq!(name_of("binutils>2.28"), "binutils");
        assert_eq!(name_of("foo<1.0"), "foo");
    }

    #[test]
    fn soname_provides_keep_the_so_name() {
        let d = Dep::parse("libalpm.so=16-64");
        assert_eq!(d.name, "libalpm.so");
        assert_eq!(d.constraint.as_deref(), Some("=16-64"));
        assert!(d.is_soname());

        assert_eq!(name_of("libGL.so=1-64"), "libGL.so");
    }

    #[test]
    fn names_with_punctuation_survive() {
        // `+` and `.` are legal in package names and must not be treated as
        // constraint starts.
        assert_eq!(name_of("libstdc++"), "libstdc++");
        assert_eq!(name_of("gcc-libs"), "gcc-libs");
        assert_eq!(name_of("lib32-at-spi2-atk=2.60.6-1"), "lib32-at-spi2-atk");
        assert_eq!(name_of("python3.13"), "python3.13");
    }

    #[test]
    fn epoch_versions_do_not_confuse_the_split() {
        assert_eq!(name_of("alsa-plugins=1:1.2.12"), "alsa-plugins");
        assert_eq!(name_of("lib32-mesa-libgl=3:26.1.6-1"), "lib32-mesa-libgl");
    }

    #[test]
    fn display_round_trips() {
        for s in ["glibc", "curl>=7.20.0", "libalpm.so=16-64", "atk<=2.38.0-2"] {
            assert_eq!(Dep::parse(s).to_string(), s);
        }
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(name_of("  glibc  "), "glibc");
    }
}
