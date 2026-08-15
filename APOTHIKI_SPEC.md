# Apothiki — Project Specification

> **Codename:** `apothiki` (Greek: *αποθήκη*, "storehouse"). Binary: `apo`.
> Rename freely — no logic depends on it.
>
> **Status:** Design spec. Decisions in §15 resolved 2026-08-14; implementation of M1 begins.
> **Target platform:** Arch Linux and derivatives. Primary dev/test machine runs **CachyOS**
> (Arch-based, rolling, KDE Plasma on Wayland, Fish shell).

---

## 0. How to read this document

This is a **why-first** spec. Section 1 explains the problem in the user's own terms.
Sections 2–5 define the conceptual model — **this is the part that matters most**, because
the hard problem here is not writing a TUI, it is deciding what an "application" *is* on a
system that has no such concept.

Do not start writing UI code until the model in §4 and §5 is understood and agreed.
The build order in §12 exists to prevent that.

**Open questions requiring user input are marked `⚠️ DECIDE`.** Do not guess on these.

---

## 1. Why this exists

The user's stated problem, verbatim in spirit:

> "I don't know what is what and what I have installed on my computer. I only see thousands
> of packages, I don't know which depends on which, which are needed and which aren't."

This is the universal Arch experience after ~1 year of use. The system accumulates:

- Packages installed explicitly, then forgotten
- Packages pulled in as dependencies of things long since removed
- Build dependencies from AUR packages that were never cleaned
- Optional dependencies installed manually with no record of why
- Metapackage fallout (installing `plasma-meta` brings ~200 packages)

`pacman -Q` returns a flat, alphabetised list of ~1500–2500 opaque identifiers.
`libwacom`, `gsettings-desktop-schemas`, `qt6-declarative`. None of these are "things the
user installed". They are implementation details of things the user installed.

**The core insight driving this project:**

> pacman has no concept of an *application*. It only has *packages*. The gap between
> "the 2000 things on my disk" and "the 40 programs I actually use" is the entire product.

Everything else — the search, the deletion, the cleanup — is table stakes that other tools
already do. The app model is the differentiator.

### 1.1 What the user asked for, explicitly

1. Show my **applications** (and AppImages if possible)
2. Let me see an *installed application*, not a random package name
3. Let me see which packages each app needs/uses
4. Delete an app with a **key press, not a command**
5. Delete a package with a key press
6. Clean up unneeded packages
7. Search and install without typing commands — start typing `dis`, see matching packages
   from both official repos and AUR, with excellent (fuzzy) search that is **instant**
8. Very fast and lightweight (Rust)
9. Noob user protection
10. CUA bindings

### 1.2 Deferred (explicitly out of scope for v1)

- Showing where each application's files live (configs, `/srv`, etc.) and explaining the
  logic of each directory. The user flagged this as "extra, ignore for now."
  See §14 for notes so it isn't designed out of possibility.

---

## 2. Non-goals

Stating these prevents scope creep, which is the primary failure risk here.

- **Not a pacman replacement.** We shell out to `pacman`/`paru` for all mutations. We never
  write to the pacman DB ourselves. Ever.
- **Not a system-wide package manager abstraction.** No apt/dnf/zypper. Arch only.
  The data layer should be *trait-abstracted* so it could grow, but do not build for it.
- **Not a GUI.** Terminal only.
- **Not a package build tool.** No PKGBUILD editing, no makepkg orchestration.
- **Not an AUR helper.** We *drive* an existing helper (`paru` or `yay`); we do not
  reimplement AUR building, `.SRCINFO` parsing, or dependency resolution for AUR.
- **Not a system updater** in v1. `-Syu` is a separate concern with its own failure modes
  (partial upgrades, keyring, news). Explicitly excluded.

---

## 3. Prior art, and the gap

Research was done. These exist and none of them solve the stated problem.

| Tool | Language | What it does | Why it's insufficient |
|---|---|---|---|
| **pacseek** | Go | TUI search + install/remove for repos and AUR; PKGBUILD preview; upgrade view | Package-centric. No app model. No dependency navigation. No impact analysis before removal. |
| **Pacsea** | Rust/ratatui | Fast search, install queue, distro-aware (detects CachyOS), AUR security scanning | Same core limitation — it's an *installer*, not a *system explorer*. |
| **pacfinder** | C/GTK3 | Package explorer with filters (explicit / dependency / orphan / foreign), dependency relationship navigation | Closest to the exploration goal, but: GUI, **read-only** (cannot remove anything), and still package-centric. |
| **pacui**, **apf** | Bash + fzf | fzf wrappers over pacman/yay/flatpak | Shell scripts. No structured model, no safety layer, no app concept. |
| **pactree**, **paclist**, **pacgraph**, **PacVis** | mixed | Individual dependency-inspection primitives | Building blocks, not a tool. |
| **octopi**, **pamac** | C++/Vala GUI | Full graphical package managers | GUI; still present packages, not applications; pamac has a poor reliability reputation on pure Arch. |

**The gap:** nothing combines *application-level view* + *impact-aware deletion* +
*instant fuzzy install* in one place. That combination is the product.

**Corollary:** we should shamelessly steal ideas. pacfinder's filter taxonomy is good.
Pacsea's privilege-escalation handling is good. Read their approaches before reinventing.

---

## 4. The core model: Package vs. Application

This section is the heart of the spec.

### 4.1 Definitions

- **Package** — a unit in the pacman database. Has a name, version, install reason
  (explicit/dependency), a `depends` list, an `optdepends` list, a `provides` list, a file
  list, and an install size. This is ground truth. It always exists.

- **Application** — a *synthesised* concept. Something a human would name when asked
  "what programs do you have?" It is derived from evidence, not stored anywhere.
  An Application maps to **one or more** packages (e.g. `firefox`; or `gimp` +
  `gimp-help-en`) and has a human name, a description, an icon, a category, and a launch
  method.

- **Tool** — an explicitly-installed package with no launchable evidence. `ripgrep`,
  `ffmpeg`, `docker`. These are user *choices* and must be visible, but they are not
  "applications" and belong in a separate view.

- **Dependency** — a package installed as a dependency with no launchable evidence.
  Invisible plumbing. Collapsed by default.

- **Orphan** — a package installed as a dependency that nothing currently requires.

Every installed package lands in exactly one of: App-backing, Tool, Dependency, Orphan.
This four-way split is what turns 2000 unreadable rows into ~40 + ~80 + collapsed.

### 4.2 App discovery: layered resolution

Four evidence layers, in **descending order of trust**. Higher layers win on conflict.

```
Layer 0  pacman local DB          — ground truth: what is actually installed
Layer 1  local metainfo (AppStream) — /usr/share/metainfo/*.xml  [HIGHEST TRUST]
Layer 2  .desktop entry scan        — XDG spec locations
Layer 3  repo AppStream catalog     — /usr/share/swcatalog/ [enrichment only]
Layer 4  heuristics                 — Tools, AppImages, Flatpak
```

#### Layer 1 — local metainfo (highest trust, do this first)

Scan `/usr/share/metainfo/*.xml` (and legacy `/usr/share/appdata/`).

**Why this is the strongest signal:** these files were installed *by a package*, so the
owning package is known exactly from pacman's file database. Zero guessing. The upstream
author wrote the name and summary. Per the AppStream spec, every component carries at
minimum `id`, `name`, `summary`, and — in catalog form — `pkgname`.

Parse from each `<component type="desktop-application">`:
- `<id>` → stable app identity (reverse-DNS, e.g. `org.gnome.Meld`)
- `<name>`, `<summary>` → display strings
- `<launchable type="desktop-id">` → **the official bridge to the `.desktop` file.**
  Use this to join Layer 1 and Layer 2 and to deduplicate.
- `<categories>` → classification

Also handle `type="console-application"` (a CLI tool that ships metainfo) and
`type="addon"` (belongs *to* another component — do not surface as a top-level app;
attach it to its parent via `<extends>`).

#### Layer 2 — .desktop scan (the workhorse)

Scan, in XDG precedence order (later overrides earlier by desktop-file-id):

```
/usr/share/applications/
/usr/local/share/applications/
~/.local/share/applications/
/var/lib/flatpak/exports/share/applications/
~/.local/share/flatpak/exports/share/applications/
/var/lib/snapd/desktop/applications/          # only if snapd is present
```

Also respect `$XDG_DATA_DIRS` / `$XDG_DATA_HOME` rather than hardcoding, since Plasma and
some Nix/Home-Manager setups add paths.

**Mandatory filters — without these the list is garbage:**

| Condition | Action |
|---|---|
| `Type != Application` | skip |
| `NoDisplay=true` | skip (this is how `avahi-discover`, `qv4l2`, MIME handlers hide) |
| `Hidden=true` | skip (tombstone — user deleted it) |
| `Terminal=true` | route to **Tools**, not Apps |
| `TryExec=` present and the binary is not resolvable in PATH | skip — every launcher hides these; it is how packages ship entries for optional components |
| `OnlyShowIn` present and doesn't match current desktop | de-prioritise, don't hard-skip |
| `NotShownIn` matches current desktop | skip |
| filename matches known-noise patterns | skip |

Known-noise examples worth a starter denylist: `*-url-handler.desktop`,
`org.kde.kwin.*`, `nvim.desktop`/`vim.desktop` (editors shipping stub entries),
`xterm.desktop`, `bssh.desktop`, `bvnc.desktop`, `lstopo.desktop`, `cmake-gui` variants
depending on taste. Keep this list in config, not code.

**Then map each surviving `.desktop` to its owning package.** See §5.1 for the reverse
index. Do **not** shell out to `pacman -Qo` per file — that is ~300 subprocess spawns.

#### Layer 2a — the desktop launcher is the same source (validation + free categories)

The user's application launcher (KDE Kickoff, GNOME Shell) already solves the "show me my
programs" problem. It is worth stating explicitly **how**, because it validates Layer 2 and
gives us a feature for free.

**KDE:** `kbuildsycoca6` scans `$XDG_DATA_DIRS/applications/` and
`$XDG_DATA_HOME/applications/`, applies the `NoDisplay` / `Hidden` / `NotShowIn` filters,
organises entries into a tree, and writes a binary cache to `~/.cache/ksycoca6_*`.

**GNOME:** identical inputs and filters via `GAppInfo`/GIO, without a cache layer.

**XFCE, Cinnamon, MATE, LXQt, and standalone launchers** (rofi, wofi, fuzzel, albert):
same spec, same paths, same filters.

**Conclusion: the start menu is exactly the Layer 2 scan.** There is no hidden database on
any desktop. This is confirmation that Layer 2 is correct, not a new data source — and it
means our app discovery is desktop-environment agnostic by construction.

**Do not read `ksycoca` directly.** It is an undocumented binary format that changes with
each major KDE release (ksycoca5 → ksycoca6) and would couple us to KDE. Scan the
`.desktop` files ourselves — same result, portable, ~50 lines.

**Do not parse `/etc/xdg/menus/*.menu` either.** Those files are DE-specific presentation
structure (`plasma-applications.menu` vs `gnome-applications.menu`). Use the `Categories=`
field from each `.desktop` directly — it is standardised, present in the file itself, and
identical across desktops. This gives free grouping into Development / Graphics / Game /
AudioVideo / Office / Network / System, matching what the user already sees in their
launcher. Use the freedesktop **registered main categories** and fold the long tail of
additional categories into their parents rather than rendering 60 groups.

**The only genuinely DE-dependent logic is `OnlyShowIn` / `NotShowIn` evaluation:**

- `$XDG_CURRENT_DESKTOP` is a **colon-separated list**, not a single value.
  Cinnamon reports `X-Cinnamon`; Ubuntu's GNOME reports `ubuntu:GNOME`.
  Split on `:` and match if *any* element matches.
- Under a tiling WM (Hyprland, sway, i3) the variable may be **unset or unrecognised**.
  In that case do **not** hide everything carrying an `OnlyShowIn` — de-prioritise instead.
  Hiding on an unknown desktop would silently drop half the list.

This is roughly ten lines of code and is the entire portability surface.

**Useful as a correctness oracle during development:**
- In the launcher but missing from our Apps view → **bug in our scan or filters**
- In the launcher but owned by no pacman package → Flatpak or AppImage (Layer 4 exists
  precisely for these)
- In our view but not the launcher → we failed to apply a `NoDisplay`/`NotShowIn` filter

#### Layer 3 — repo AppStream catalog (enrichment only, never authority)

`/usr/share/swcatalog/xml/*.xml.gz` and `/usr/share/swcatalog/icons/`, provided by the
`archlinux-appstream-data` package. Also check `/var/lib/swcatalog/` and
`/var/cache/swcatalog/`.

**Use it for:** icons, richer summaries, categories, and screenshots for packages that are
*not yet installed* (i.e. in the search/install view).

**Never use it as the source of truth for "what I have installed", because:**

1. It is a **snapshot of the repositories**, generated periodically. It describes what
   *exists*, not what *you have*.
2. **Zero AUR coverage.** The user definitely has AUR packages.
3. **Partial coverage.** Only packages that ship metainfo appear. Per AppStream's own
   validation docs, a component missing a `desktop-id` launchable tag is *ignored entirely*
   by the generator — so real GUI apps are silently absent from the catalog.
4. **Zero AppImage coverage.**
5. It can be stale relative to a rolling system, or the package may not be installed at all.

If `archlinux-appstream-data` is absent, everything must still work. Degrade to Layers 1+2.

#### Layer 4 — heuristics

**Tools:** packages that are `explicit`, have no launchable evidence, and are not in the
`base`/`base-devel` groups. Rank by whether they own something in `/usr/bin`.

**AppImages:** no registry exists anywhere. Purely filesystem discovery.

**Follow `Exec=` first; treat directory scanning as the fallback.** Measured: the directory
list below finds **zero** of the four AppImages on the dev machine, which keeps them in
`~/AppImages` — a directory this spec originally omitted. Any "usual directories" list is a
guess about a user's filing habits, whereas an integrated `.desktop` entry points at the
file wherever it actually is.

- **Primary:** for each launchable that no package owns, resolve the `Exec=` target and test
  it for a `.AppImage` extension (case-insensitive) plus the executable bit. Must see
  through `env VAR=x /path/to/app` wrappers — nine user entries on this machine launch that
  way — and through quoted paths.
- **Secondary:** scan `~/AppImages`, `~/Applications`, `~/.local/bin`, `~/Downloads`,
  `~/bin`, `/opt` to catch AppImages never integrated into a launcher. Keep the list in
  config. Check the executable bit: a non-executable `.AppImage` is a download that was
  never run, not an installed application.
- Optionally read the embedded `.desktop` from the AppImage's squashfs for a proper name.
  This requires either `--appimage-extract` (slow, writes to disk) or parsing the ELF +
  squashfs offset directly. **v1: skip extraction, use filename + any integrated desktop
  entry.** Extraction is a v2 nice-to-have.
- AppImages have **no dependency graph** — they are self-contained. The dependency panel
  must show "self-contained bundle", not an empty list that looks like a bug.
- Deletion = `rm` the file + remove the integrated `.desktop` + remove the icon +
  optionally offer to remove `~/.config/<name>`. Confirm each separately.

**Flatpak:** proper CLI exists. `flatpak list --app --columns=application,name,size,origin`.
Uninstall via `flatpak uninstall`. Cleanup via `flatpak uninstall --unused`.
Treat as a parallel source with its own removal path. Low risk, high value, cheap.

Flatpak apps already appear in the Layer 2 scan, since their exports sit in
`XDG_DATA_DIRS`. So this is not a discovery problem but an **attribution** one: match the
exported `<app-id>.desktop` to mark the entry Flatpak-owned instead of "no package owns
this". Keep the reported size as text — the CLI localises the decimal separator (`395,7 MB`
here), so parsing it into bytes is wrong in exactly the locales where it looks parseable.

**Steam shortcuts.** Not in the original spec, and unavoidable: 13 of the launchables here
are Steam library entries whose `Exec` is `steam steam://rungameid/…`. They are not
installed software in any package sense and Steam owns their lifecycle. Give them their own
source so they can be grouped or hidden, and never offer removal. Without this they sit next
to Firefox with no owning package, which reads as a bug in our attribution.

**Leave a genuine "unknown" bucket.** Hand-extracted tarballs exist (one here: Telegram in
`~/Documents/Apps`). A source of last resort that admits we don't know beats a heuristic
that quietly mislabels it.

### 4.3 Rejected alternatives (do not revisit)

| Approach | Why rejected |
|---|---|
| **Filter by `pacman -Qe` (explicit only)** | **Actively wrong on the target system.** On CachyOS/KDE, most applications arrive via metapackages (`plasma-meta`, `kde-applications-meta`) and are marked as *dependencies*. This would hide half the user's apps. This is the single most important negative finding. |
| **Use pacman groups** | Far too coarse. `kde-applications` is one group with hundreds of members. |
| **Anything owning a file in `/usr/bin`** | Enormous noise. `glibc`, `coreutils`, and every library's helper binaries qualify. |
| **AppStream catalog as primary** | See §4.2 Layer 3 — snapshot, no AUR, partial coverage. |
| **Hardcoded curated app list** | Unmaintainable, breaks for AUR and anything niche. |

---

## 5. Data access

### 5.1 Reading the pacman database

**✅ DECIDED: hand-rolled parser.** Rationale:

- The `alpm` crate is FFI bindings to `libalpm`. On a rolling distro, a pacman major
  release bumps the libalpm soname and **the crate stops compiling / the binary stops
  running until the crate catches up**. This has happened repeatedly. It means the tool
  breaks on a random Tuesday, which is exactly the opposite of what a "system health" tool
  should do.
- The on-disk format is simple, stable, and plain text.
- Parsing ~2000 packages is a few tens of milliseconds. There is no performance argument
  for FFI here.

**Format reference:**

```
/var/lib/pacman/local/<pkgname>-<version>/
    desc      # metadata: %NAME% %VERSION% %DESC% %DEPENDS% %OPTDEPENDS%
              # %PROVIDES% %REASON% %SIZE% %INSTALLDATE% %GROUPS% %CONFLICTS%
              # %REPLACES% %URL% %LICENSE% %ARCH% %PACKAGER% %VALIDATION%
    files     # %FILES% then one relative path per line, then optional %BACKUP%
    mtree     # compressed; ignore
```

Format: a `%KEY%` line, then values one per line, then a blank line. Trivially parsed.
`%REASON% 1` means "installed as dependency"; absent means explicit.

`%BACKUP%` in `files` lists config files pacman tracks for user modification — keep this,
it's the foundation of the deferred feature in §14.

```
/var/lib/pacman/sync/<repo>.db      # compressed tar of desc files for available packages
```

Read these for the install/search view. Same `desc` format inside. Compression varies per
repo — see §9; sniff, do not assume.

**Detecting foreign (AUR/local) packages — measured, not assumed.**
`%INSTALLED_DB%` in a local `desc` names the repo a package was installed from, but **only
newer pacman versions write it**. On the dev machine 520 of 1656 packages lack the field
while just 11 are genuinely foreign, so *absence proves nothing*. The reliable test is
**"the name appears in no sync database"** — that reproduces `pacman -Qm` exactly.
Use `%INSTALLED_DB%` as an authoritative-when-present origin hint, never as a foreign test.
Note it can also name a repo that no longer exists in `pacman.conf`.

**Repo shadowing is the norm, not an edge case:** 1191 of 1656 installed packages exist in
more than one configured repo. The repo column in the detail pane (§11) is therefore
load-bearing for most of the list, not a rarity.

**Build a reverse file index once:** `HashMap<PathBuf, PkgId>` from all `files` lists.
~2000 packages × ~250 files ≈ 500k entries. A few hundred ms cold. Cache it to
`$XDG_CACHE_HOME/apothiki/fileindex.bin` (bincode/postcard), invalidated by the mtime of
`/var/lib/pacman/local`. This index is what makes `.desktop → package` resolution instant.

**Handle `db.lck`:** if `/var/lib/pacman/db.lck` exists, another pacman is running.
Detect it, show a clear banner, disable mutations. Do not crash, do not hang.
Re-check on a timer so the UI recovers automatically when the other process finishes.

### 5.2 The dependency graph

Three distinct questions the UI must not conflate:

1. **Depends On (direct)** — what this package declares. 5–20 entries.
2. **Full transitive closure** — the true footprint. Often 200+ for a GTK/Qt app.
3. **Required By (reverse deps)** — **this is the one that matters for deletion.**

And the question the user *actually* wants answered:

> **"If I delete this, what goes with it and how much space do I get back?"**

That is a simulation of `pacman -Rs`: compute which dependencies become orphaned after
removing the target, recursively, excluding anything still reachable from another explicit
root. Compute this in-process from the graph for instant feedback, then **confirm with a
real dry-run before executing.** Never execute based only on our own computation.

**Graph correctness pitfalls — all of these will bite:**

- **`provides` / virtual packages.** Dependencies are frequently not package names.
  `sh`, `java-runtime`, `libGL.so=1-64`, `sh`, `awk`. You must build a
  `provides → providers` map and resolve through it, including versioned provides
  (`foo=1.2`) and soname provides (`libfoo.so=1-64`). Without this the graph has holes and
  the orphan computation is wrong.
- **Version constraints.** Strip them for dependencies on **real package names** — an
  already-consistent installed set satisfies them by definition. **Never strip them when
  resolving through `provides`.** For sonames the version *is* the identity, and the
  `-32`/`-64` suffix is the ELF class:

  ```text
  libxml2         provides libxml2.so=16-64      different libraries
  libxml2-legacy  provides libxml2.so=2-64
  glew            provides libGLEW.so=2.3-64     different architectures
  lib32-glew      provides libGLEW.so=2.3-32
  ```

  Stripping fuses the 32- and 64-bit worlds and makes every legacy compatibility package
  look permanently required, so it never appears in a removal cascade. Verified: this alone
  accounted for 3 of 5 divergences from `pacman -Rs --print` across 1656 packages.
  Compare exact-versus-exact only; range constraints on true virtuals (`java-runtime>=11`)
  are rare and resolving them permissively errs in the safe direction.

- **A real package and its providers both satisfy a dependency.** `ca-certificates-utils`
  provides `ca-certificates`, and a package of that name also exists. Treating the real
  package as the sole satisfier drops the provider's reverse edges and makes it look
  removable when seven packages still need it.

- **Removal planning must mirror alpm, not our own analysis.** Its job is to *predict
  pacman*, and pacman reasons by reference count while we reason by reachability. The two
  genuinely differ: a package kept alive only by an orphan is not an orphan to pacman, but
  removing that orphan does take it. A package joins the removal when (1) something already
  being removed depends on it, (2) it was installed as a dependency, and (3) everything
  requiring it is also being removed. Condition (1) is what leaves *pre-existing* orphans
  alone — omitting it made removing one program claim to remove fourteen unrelated packages.

- **Cycles must be removed as a group.** Inside a cycle every member is required by another,
  so a package-at-a-time rule deadlocks and the cycle is never freed. Condense the graph into
  strongly-connected components and judge each component against the world outside it. Real
  examples on the dev machine: `tesseract` ⇄ `tesseract-data-eng` (via the `tessdata`
  provide) and `python-beautifulsoup4` ⇄ `python-soupsieve`. Use iterative Tarjan; recursion
  depth follows the dependency chain.
- **`optdepends` are not edges.** They are documentation. But removing one silently
  degrades an app — no error, features just stop working. This is the #1 way users break
  things without knowing. **Surface optdepends prominently and separately**, with the
  reason string pacman stores, and warn when a removal target is an optdep of something
  installed.
- **Orphans hidden from refcounting.** `pacman -Qdt` is a **refcount**, not a garbage
  collector, so a reachability pass from the explicit-install roots finds orphans it cannot.
  Two distinct shapes end up here and the UI must not conflate them:

  - **Transitively stranded** — an orphan root with a tree beneath it. Measured on the dev
    machine: `npm` is required by nothing, yet keeps `nodejs`, `node-gyp`, `nodejs-nopt`,
    `semver`, `ada` and `simdjson` at non-zero refcounts, so `-Qdtt` reports none of them.
    This is the common case, and it is *not* a cycle.
  - **Cycle-trapped** — mutually-requiring packages, which refcounting can never free no
    matter what is removed first.

  Distinguishing them needs a real reachability test; "something requiring it is also an
  orphan" labels the first shape as the second. Both are almost always genuine garbage, but
  present them cautiously since pacman itself won't have flagged them.
- **`-Qdt` vs `-Qdtt`.** The single-`t` form excludes packages that are *optional*
  dependencies of something. The double-`t` form includes them. These are different
  safety levels. Expose both, default to the safer `-Qdt`.

---

## 6. Safety model

This is the second differentiator. Without it we are just another pacseek.

### 6.1 Hard denylist — not even confirmable

Removal must be **structurally impossible**, not merely discouraged, for:

- Everything in the `base` and `base-devel` groups
- `linux`, `linux-lts`, `linux-cachyos*`, `linux-firmware`, and matching `*-headers`
- `systemd`, `systemd-libs`, `glibc`, `gcc-libs`, `pacman`, `bash`, `coreutils`
- `mesa`, `vulkan-*`, `nvidia*`, `nvidia-utils` (the user has an RTX 2080 — removing these
  means no display on next boot)
- Bootloader packages (`grub`, `systemd-boot` via systemd, `limine`, `refind`)
- The active desktop metapackage (`plasma-meta`, `plasma-desktop`, `kde-*-meta`)
- Anything transitively required by the above

The UI should say plainly: *"This is part of the system, not something you installed."*
Do not offer a `--force` flag. If the user genuinely needs to remove `glibc`, they know
how to use pacman.

### 6.2 Risk tiers

Every removable item gets a tier, shown as colour + text:

| Tier | Meaning | Confirmation |
|---|---|---|
| 🟢 **Safe** | Leaf. Nothing depends on it. Not an optdep of anything installed. | Single keypress + summary |
| 🟡 **Caution** | Has reverse dependencies, or is an optdep of an installed package, or removal cascades to 5+ packages | Explicit confirm dialog with full cascade list |
| 🔴 **Dangerous** | Cascade includes a package that backs a visible Application, or touches a group boundary, or removes >500 MB | **Type-to-confirm**: user types the package name |
| ⛔ **Blocked** | §6.1 denylist | Not offered |

### 6.3 Impact preview (mandatory, always)

Before any removal, show:

- Full list of packages that will be removed, grouped: *target* / *cascade* / *orphaned*
- Total disk space reclaimed
- **Explicit warning naming any Applications that will disappear** — "this will also
  remove GIMP" is far more meaningful than "this will also remove `gegl`"
- Any config files (`%BACKUP%`) that will be left behind as `.pacsave`

### 6.4 Snapshots — the highest-value safety feature

The target machine is CachyOS, which ships btrfs + snapper (and limine-snapper-sync) by
default. Detect this at startup.

If present, offer a **pre-transaction snapshot toggle**, on by default. Create a snapper
snapshot with a description naming the operation before executing. This converts every
mistake from a disaster into a reboot. It is worth more than all the other safety features
combined.

Detection: check for `snapper` in PATH and a valid config via `snapper list-configs`.
Fall back gracefully — do not require btrfs.

### 6.5 Undo

Maintain a transaction log at `$XDG_DATA_HOME/apothiki/history.jsonl`: timestamp,
operation, exact package list with versions, exit status.

Because `/var/cache/pacman/pkg/` retains the `.pkg.tar.zst` files, an offline reinstall of
a just-removed set is usually possible: `pacman -U /var/cache/pacman/pkg/<exact-files>`.
Offer this as "Undo last removal" and check cache presence before promising it.
If `paccache` has pruned them, say so rather than failing mid-operation.

---

## 7. Search and install

### 7.1 Repository search — instant, trivially

Read `/var/lib/pacman/sync/*.db` at startup into memory. ~15k packages with names and
descriptions is a few MB. Fuzzy-match locally. There is no reason for this to ever be slow.

### 7.2 AUR search — the only genuine latency problem

The AUR RPC endpoint (`https://aur.archlinux.org/rpc/?v=5&type=search&arg=...`) has
rate limiting, a result cap, and 200–800 ms latency. **Hitting it per keystroke will get
you rate-limited and will not feel instant.**

**Solution: local index.**

- Download `https://aur.archlinux.org/packages-meta-ext-v1.json.gz` (~15 MB compressed)
  on first run and refresh on a schedule (daily, or on explicit user request).
- Parse into a compact local index: name, description, version, votes, popularity,
  out-of-date flag, maintainer.
- Fuzzy-search that index locally → genuinely instant, same code path as repo search.
- Use the RPC **only** when the user selects a specific package, to fetch live details
  (current votes, last modified, dependencies), debounced.

**Handle the cold-start case:** first run has no index. Don't block the UI. Search repos
immediately, show "AUR index downloading…" in the AUR section, populate when ready.

### 7.3 Fuzzy matching

Use **`nucleo`** (the matcher from Helix). It is the fastest available, handles 100k+
candidates per keystroke, and supports proper scoring with case/path awareness.
`fuzzy-matcher` (skim's `SkimMatcherV2`) is an acceptable fallback but slower.

Ranking must weight: exact name match > name prefix > name substring > description match.
Then boost installed packages, official repos over AUR, and high-vote AUR packages.
Typing `dis` should surface `discord`, not `libdiscid`.

### 7.4 Executing operations

**✅ DECIDED: (C) pre-authenticate, then stream in a PTY.** The user's requirement is that
escalation never leaves the TUI and never costs more than one password entry:
*select action → prompt for password if needed → done.*

Options (A) and (B) below are retained as rationale. (A) is rejected as the primary path
because it visibly leaves the TUI. (B) as literally specified — parsing a password prompt
out of a command's output stream — is the known foot-gun. (C) avoids it:

1. **Pre-flight.** Run `sudo -n -v`. If it succeeds (NOPASSWD, or a warm sudo timestamp),
   no prompt is shown at all. This is the common case during a session.
2. **Prompt.** If it fails, render a masked input widget *in the TUI* and feed the password
   to `sudo -S -v` (or `sudo -v` over a PTY) — validation only, running no command.
   This refreshes the sudo timestamp, cached ~5 min by default.
   Buffer must be `zeroize`d; never logged, never placed in argv, never echoed.
3. **Execute.** All privileged commands then run as `sudo -n <cmd>` inside a
   `portable-pty` PTY, with output streamed into a scrollable TUI pane
   (ANSI parsed via `ansi-to-tui` or `vt100`). No prompt detection is ever required,
   because authentication already happened in step 2.
4. **Re-auth.** If a long operation outlives the timestamp, `sudo -n` fails cleanly and we
   return to step 2 rather than hanging on an invisible prompt.

**Interactivity:** removal (M2) is fully non-interactive — our own confirm dialog replaces
pacman's, and we pass `--noconfirm` only *after* the impact preview was accepted.
AUR install (M3) is the genuinely interactive case (helper menus, PKGBUILD review,
conflict resolution); the PTY pane forwards keystrokes to the child for those.

**Fallbacks, in order:** if no usable tty for sudo → `pkexec` (KDE polkit agent is present
on the target machine); if neither → suspend-and-hand-off (A) as the last resort.
Keep (A) implemented as an escape hatch and expose it as a config option.

Original options, for reference:

**(A) Suspend-and-hand-off** — leave raw mode, restore the terminal, spawn
`sudo pacman ...` / `paru ...` inheriting stdio, wait, then re-enter raw mode and refresh
state.
*Pros:* trivial; sudo password, PAM, fingerprint, and `paru`'s own interactive prompts all
just work; all output is visible and scrollback-able.
*Cons:* screen flashes; not "inside" the TUI.

**(B) PTY streaming** — use `portable-pty`, run the command in a pseudo-terminal, parse and
render output inside a TUI pane with a progress bar.
*Pros:* polished, cohesive.
*Cons:* significantly more work. Must handle password prompts, `paru`'s interactive
selection menus, ANSI sequences, and progress bars. Getting sudo password entry right
inside a TUI is a known source of bugs and security foot-guns.

*(Superseded by (C) above.)*

**Never run the TUI itself as root.** Read-only inspection needs no privileges at all.

---

## 8. Keybindings

**✅ DECIDED: Common User Access** — the Ctrl+C/V/X/Z/F, F1-for-help convention familiar
from GUI applications, as opposed to vim-style `hjkl` modal navigation. Consistent with the
"noob protection" requirement. (Not Computer Use Agent.)

**Target terminal note:** the primary machine runs KDE Konsole (`TERM=xterm-256color`),
which does **not** support the Kitty keyboard protocol. The fallback binding set is
therefore the *primary* path on the dev machine, not a degraded mode — design and test it
first, then treat protocol-enhanced bindings as the enhancement.

### 8.1 The terminal conflict problem

Terminals intercept most CUA chords. This is a real constraint, not a detail:

| Chord | Conflict |
|---|---|
| `Ctrl+C` | SIGINT |
| `Ctrl+Z` | SIGTSTP (suspend) |
| `Ctrl+S` / `Ctrl+Q` | XON/XOFF flow control — freezes the terminal |
| `Ctrl+M` | indistinguishable from Enter in legacy mode |
| `Ctrl+I` | indistinguishable from Tab |
| `Ctrl+H` | indistinguishable from Backspace |

**Mitigations:**
- Raw mode via crossterm handles SIGINT/SIGTSTP interception.
- Disable flow control (equivalent of `stty -ixon`) so `Ctrl+S` is usable.
- Enable the **Kitty keyboard protocol** (crossterm supports
  `PushKeyboardEnhancementFlags`). This gives unambiguous key events and makes
  `Ctrl+I` ≠ `Tab` work correctly. Supported by kitty, foot, ghostty, WezTerm, and
  Alacritty (with config).
- **Detect support at runtime and provide fallback bindings.** Never assume.
- Always restore terminal state on exit, panic, and signal. Install a panic hook that
  leaves raw mode — a TUI that wrecks the terminal on crash is unforgivable.

### 8.2 Bindings (revised after use, 2026-08-15)

The original F-key scheme was tried and revised by the user. Views moved to the
number row, freeing the F-keys for their conventional meanings, and
type-to-search was dropped because it conflicts with digits as view keys.

```
Views           1 Apps / 2 Tools / 3 Dependencies / 4 Orphans  (F2-F6 alias)
Navigation      Arrows, PgUp/PgDn, Home/End
Descend         → or Enter   list → relationships → jump to that package
Ascend          ← or Backspace
Search          Ctrl+F only — typing does not start a search
Remove          Del, or the action at the top of the relationships pane
Orphan cleanup  c  (Orphans view)
Orphan level    Space toggles -Qdt / -Qdtt
Help / Refresh  F1 / F5
Quit            Ctrl+Q
```

**Right/Enter lands on the first relationship, not the removal action.**
Descending must never put a destructive option under the cursor; the user
reaches it deliberately, by pressing Up.

### 8.2a Original proposal (superseded)

```
Navigation      Arrows, PgUp/PgDn, Home/End, Tab / Shift+Tab between panes
Search          Ctrl+F  (or just start typing in list views)
Help            F1
Quit            Ctrl+Q  (with confirm if an operation is queued)
Delete          Del  (opens impact preview; never deletes immediately)
Confirm         Enter
Cancel          Esc
Undo            Ctrl+Z  (transaction undo, §6.5)
Refresh         F5
Views           F2 Apps / F3 Tools / F4 Dependencies / F6 Orphans / F7 Search
Toggle detail   Space
```

Show a persistent key hint bar at the bottom. Discoverability *is* the noob protection.

---

## 9. Technology stack

| Concern | Choice | Notes |
|---|---|---|
| TUI framework | `ratatui` + `crossterm` | de facto standard; note the 0.30 workspace split — depend on the main `ratatui` crate |
| Fuzzy matching | `nucleo` | fastest available |
| Pacman DB | hand-rolled parser | see §5.1 |
| Sync DB (tar.*) | `flate2` + `zstd` + `tar` | **Not gzip-only.** The `.db` extension is uniform and tells you nothing: Arch's `core`/`extra`/`multilib` are gzip, all four CachyOS repos are **zstd**. Sniff the magic bytes (`1f 8b` / `28 b5 2f fd`). Assuming gzip silently drops whole repositories, which then reads downstream as "these packages are foreign" — a wrong answer that looks plausible. |
| AppStream XML | `quick-xml` | streaming; do not pull in the `appstream` C library |
| Serialisation/cache | `serde` + `postcard` or `bincode` | |
| Errors | `anyhow` (app) + `thiserror` (lib) | |
| Async | **avoid if possible** | only the AUR index download needs it. Consider `ureq` (blocking) on a worker thread instead of pulling in tokio. Keeps the binary small and compile times low. |
| Config | `toml` + `serde` at `$XDG_CONFIG_HOME/apothiki/config.toml` | |
| Logging | `tracing` to a file, never to stdout | stdout is the TUI |

**Build profile:** LTO, `codegen-units = 1`, `panic = "abort"` for release,
`strip = true`. Target a binary under 5 MB and cold start under 100 ms.

---

## 10. Architecture

```
src/
  main.rs              # arg parsing, terminal setup/teardown, panic hook
  app.rs               # application state, event loop
  config.rs

  data/
    mod.rs             # PackageDb trait — the abstraction seam
    local.rs           # /var/lib/pacman/local parser
    sync.rs            # /var/lib/pacman/sync parser
    fileindex.rs       # reverse path → package index + caching
    graph.rs           # dependency graph, provides resolution, orphan detection,
                       # removal-impact simulation
    aur.rs             # AUR local index + RPC client

  apps/
    mod.rs             # App model, layered resolution orchestration
    metainfo.rs        # Layer 1
    desktop.rs         # Layer 2
    catalog.rs         # Layer 3
    appimage.rs        # Layer 4
    flatpak.rs         # Layer 4

  ops/
    mod.rs             # Operation enum, planning, execution
    safety.rs          # denylist, risk tiers, impact preview
    snapshot.rs        # snapper integration
    history.rs         # transaction log, undo
    exec.rs            # privilege escalation, command spawning

  ui/
    mod.rs
    views/             # apps, tools, deps, orphans, search
    widgets/           # detail pane, impact dialog, confirm dialog, keybar
    theme.rs
```

**Data flow:** all filesystem/DB reads happen on a background thread at startup and on
refresh, producing an immutable `SystemState` snapshot. The UI thread only reads it.
Never do I/O in the render loop. Never block on the network in the render loop.

**Caching:** file index and AUR index cached to `$XDG_CACHE_HOME/apothiki/`.
Invalidate the file index on `/var/lib/pacman/local` mtime change. Version the cache
format so a stale cache from an older build is discarded, not misparsed.

---

## 11. Distro-specific handling

The primary machine is CachyOS. Do not hardcode assumptions, but do handle:

- **Extra repos:** `cachyos`, `cachyos-v3`, `cachyos-v4`, `cachyos-extra`, `cachyos-core`.
  These contain *optimised replacements* for upstream Arch packages with identical names.
  **The detail pane must always show which repo a package came from** — otherwise the
  user cannot tell whether they have Arch's `ffmpeg` or CachyOS's.
- **AUR helper detection:** prefer `paru`, fall back to `yay`, then `pikaur`. Make it
  configurable. Detect at startup, warn if none found (install view degrades to repo-only).
- **Snapper/limine** presence for §6.4.
- Other derivatives (EndeavourOS, Manjaro, Artix) should work without special-casing, but
  Manjaro's delayed repos mean version comparisons against Arch upstream are meaningless —
  don't build features that assume Arch's release cadence.

---

## 12. Build order

Each milestone must be independently useful and independently shippable.

### M1 — Read-only explorer  ← **start here**

**Zero write operations. Nothing that touches the system.** This alone solves the user's
original stated problem.

- Local DB parser + reverse file index + caching
- Dependency graph with `provides` resolution and cycle detection
- App resolution: Layers 1, 2, and 4-Tools
- Four views: Apps / Tools / Dependencies / Orphans
- Detail pane: description, version, repo, size, install reason, install date
- Dependency panel: Depends On / Required By / Optional (three clearly separated lists)
- **Impact preview** — "if you removed this, N packages and X MB would go" (displayed
  only; no removal capability)
- Navigation between related packages (jump to a dependency, jump back)

**Acceptance:** cold start under 200 ms on a 2000-package system. The Apps view shows
something the user recognises as "my programs". Verified by the user actually using it.

**Testing:** snapshot a real `/var/lib/pacman/local` tree into `tests/fixtures/` and assert
parse results and graph properties against it. Include a fixture with a dependency cycle
and one with versioned/soname provides.

### M2 — Removal

- Safety layer: denylist, risk tiers, impact preview dialog
- Snapper detection and pre-transaction snapshots
- Removal execution via privilege strategy (A)
- Orphan cleanup with `-Qdt` / `-Qdtt` distinction
- Transaction history + undo-from-cache
- `db.lck` handling

**Acceptance:** attempting to remove `glibc` is impossible. Removing a leaf app is one
keypress plus one confirm. A snapshot exists afterwards.

**Testing (per §15.6):** the agent performs **no real removals**. Every removal plan is
verified against `pacman -Rs --print` (read-only, needs no root) and asserted equal to our
in-process simulation. Divergence between the two is a bug in the graph, and this
comparison is the single most valuable test in the project — run it across many packages,
not just a few. The user executes the first real removal, snapshot first.

### M3 — Install and search

- Sync DB parsing + `nucleo` fuzzy search
- AUR local index with background download and refresh
- Unified ranked results across repos and AUR
- Package detail for not-yet-installed packages (AppStream catalog enrichment)
- PKGBUILD preview for AUR before install
- Install execution via helper

**Acceptance:** typing `dis` shows `discord` at the top within one frame.

### M4 — Flatpak and AppImage

- Flatpak source adapter (list, uninstall, `--unused` cleanup)
- AppImage discovery and removal

### M5 — Polish

- Config file, theming, keybinding customisation
- Kitty keyboard protocol with fallback
- Help overlay
- Packaging: PKGBUILD, AUR submission

---

## 13. Known pitfalls — checklist

Ordered roughly by likelihood of causing a bug.

1. `provides` not resolved → holes in the graph → wrong orphan detection
2. `NoDisplay`/`Hidden` not filtered → app list full of junk
3. `pacman -Qo` called per file → startup takes 30 seconds
4. Filtering by `explicit` → half the user's apps disappear (see §4.3)
5. AUR RPC called per keystroke → rate limited, feels slow
6. Dependency cycles → orphans that can never be found
7. `optdepends` treated as removable with no warning → silent feature loss
8. `db.lck` ignored → confusing failures when another pacman runs
9. Terminal state not restored on panic → wrecked terminal
10. `alpm` crate ABI break → tool doesn't compile after a pacman update
11. Version constraints in `depends` not stripped → no edges match
16. Version constraints stripped from **provides** → 32-bit and legacy packages fused with
    their 64-bit or current counterparts, and never removable
17. Removal cascade computed from reachability rather than alpm's refcount rule →
    pre-existing orphans swept into unrelated removals
18. Dependency cycles judged one package at a time → cascade deadlocks, and pacman removes
    packages we promised would stay
12. Multi-package apps not merged (`gimp` + `gimp-help-*`) → duplicate rows
13. AppImage panel shows an empty dependency list → looks broken
14. Cache not versioned → old cache misparsed after a format change
15. Assuming `archlinux-appstream-data` is installed → crash on a minimal system

---

## 14. Deferred: file locations and config discovery

Out of scope for v1. Notes so the design doesn't preclude it:

- **Tracked config files are already known.** The `%BACKUP%` array in each package's
  `files` entry lists exactly the files pacman treats as user-modifiable config.
  Cross-reference with on-disk hashes to find which the user has actually changed.
- **Package file lists** come free from the index we already build.
- **User configs** follow XDG: `~/.config/<name>`, `~/.local/share/<name>`,
  `~/.cache/<name>`. Matching requires name heuristics (package name, binary name,
  AppStream ID, `.desktop` id) — imperfect but usually right.
- **"What does each directory do"** is static FHS knowledge, not something discoverable.
  It would be a curated dataset shipped with the tool, not a computed feature. Worth doing
  as a small help overlay eventually; not a v1 feature.

---

## 15. Resolved decisions (2026-08-14)

1. **CUA bindings** → **Common User Access** (§8). Konsole is the target terminal; the
   non-Kitty fallback binding set is the primary path.
2. **`alpm` crate vs. hand-rolled parser** → **hand-rolled** (§5.1).
3. **Project name** → `apothiki` / binary `apo` retained.
4. **Privilege strategy** → **(C) pre-authenticate via `sudo -v`, then stream in a PTY**
   (§7.4). Never leaves the TUI; at most one password entry per sudo timestamp window.
5. **Multi-package apps** → **conservative suffix match**: merge only when a package name is
   `<app>-<suffix>` for a known suffix set (`-docs`, `-doc`, `-help-*`, `-lang-*`, `-i18n`,
   `-l10n`, `-data`, `-common`, `-icons`, `-themes`) **and** the package has no launchable
   evidence of its own. Suffix set lives in config, not code. A visible duplicate row is an
   acceptable failure; a wrongly-merged app is not.
6. **Destructive-operation testing** → fixtures + dry-run only. The agent runs no real
   removals; `pacman -Rs --print` verifies every plan. The user drives the first real
   removal, with a snapper snapshot taken first.

## 15a. Measured profile of the target machine (2026-08-14)

Verified, not assumed. Re-measure rather than trusting these if behaviour looks off.

| Fact | Value | Consequence |
|---|---|---|
| Installed packages | 1656 | Well under the 2000 assumed in perf targets |
| Explicit | 281 | vs. 259 `.desktop` files — confirms §4.3: explicit-filtering is not the app list |
| Orphans (`-Qdt`) | 4 | Orphan view will look empty; that is correct, not a bug |
| metainfo XML | 95 | Layer 1 covers ~37% of desktop entries at best |
| `.desktop` (system / user) | 232 / 27 | Layer 2 is the workhorse, as predicted |
| `archlinux-appstream-data` | **not installed** | `/usr/share/swcatalog/` is empty. The Layer 3 degrade path is the **default** path on this machine, not an edge case. Test it first. |
| Repos | `cachyos-v3`, `cachyos-core-v3`, `cachyos-extra-v3`, `cachyos`, `core`, `extra`, `multilib` | Same-name packages from CachyOS and Arch coexist — the repo column is mandatory (§11) |
| AUR helpers | `paru` **and** `yay` | Prefer `paru`; make configurable |
| Root filesystem | btrfs, `snapper` present | §6.4 snapshots are live, not hypothetical |
| Desktop / terminal | KDE Plasma / Konsole (`xterm-256color`) | No Kitty keyboard protocol; polkit agent available for the `pkexec` fallback |
| AppImages found | 0 | M4 AppImage work has no local test data — needs a synthetic fixture |
| Rust | 1.97.1 | — |

---

## 16. Guidance for the implementing agent

- **Do not start with the UI.** Build `data/` and `apps/` with tests against real fixture
  data. If the model is wrong, no amount of UI polish saves it.
- **Never write to the pacman database.** All mutations go through `pacman`/`paru`.
- **Never run as root.**
- **Test destructive operations in a container or VM.** The target machine is the user's
  daily driver.
- **When something is ambiguous, ask.** The `⚠️ DECIDE` markers are non-negotiable.
- **Prefer boring, obvious code.** This tool's value is trustworthiness. A clever
  abstraction that obscures why a package was classified as an orphan is a liability.
- **Every classification decision must be explainable in the UI.** If we call something an
  orphan, the user must be able to see why. "Trust me" is not acceptable for a tool whose
  main function is deleting things.
