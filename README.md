# apothiki

An application-centric package explorer for Arch Linux. Binary: `apo`.

pacman has *packages*; you have *applications*. `pacman -Q` returns two thousand
opaque identifiers — `libwacom`, `gsettings-desktop-schemas`, `qt6-declarative` —
none of which are things you installed. They are implementation details of things
you installed. Closing that gap is the point of this program.

Every installed package lands in exactly one of four buckets: **App-backing**,
**Tool** (something you chose that has no launcher), **Dependency**, or
**Orphan**. On a typical system that turns ~1650 unreadable rows into ~110
applications, ~220 tools, and a lot of collapsed plumbing.

## Views

| Key | View | |
|---|---|---|
| `1` | Apps | what you would call a program, with its icon, size and backing packages |
| `2` | Tools | explicitly installed, no launcher — `ripgrep`, `ffmpeg`, `docker` |
| `3` | Dependencies | the plumbing, browsable when you need it |
| `4` | Orphans | installed as a dependency, needed by nothing |
| `5` | Search | repositories and the AUR, as you type |
| `6` | Updates | everything with a newer version, applications first |

`Tab` cycles views. `→`/`Enter` goes deeper, `←`/`Backspace` comes back.

## Keys

```
f            filter the current view / edit the search query
Del          remove the selected application or package
l            where this package's files live
u            updates
Ctrl+Z       undo the last removal, from the package cache
F1           help          F5   refresh          q   quit
```

All rebindable — see `apo config`.

## Safety

This tool deletes things, so it is built to be wrong loudly rather than quietly.

- **The impact preview names applications, not just packages.** "This will also
  remove GIMP" is meaningful in a way that "this will also remove `gegl`" is not.
- **Every plan is checked against pacman before it runs.** The in-process
  simulation is compared with `pacman -Rs --print`, and a disagreement aborts the
  operation. It currently matches on all 1656 packages of the development system.
- **Some removals are structurally impossible.** Kernels, `glibc`, `systemd`,
  `pacman`, the bootloader, the graphics stack, the active desktop, and anything
  transitively required by them. There is no `--force`, and no config setting can
  add one.
- **A snapper snapshot is offered first** when snapper is present, on by default.
- **Nothing is written to the pacman database.** Every mutation shells out to
  pacman, flatpak or an AUR helper.

## Diagnostics

```sh
apo doctor              check the removal pipeline end to end
apo denylist <pkg>      why a package cannot be removed
apo verify [n|names…]   compare the removal model against pacman itself
apo search <query>      ranked search from the command line
apo icon <name>         why an icon did not load
apo config              write a commented example configuration
```

`apo verify` is the important one. It compares our answers to pacman's on
removals and orphan detection, using only read-only commands. A divergence means
the tool would tell you something untrue about deleting your system.

## Configuration

`$XDG_CONFIG_HOME/apothiki/config.toml`, all optional. Run `apo config` to write
a commented example. Covers app-merging suffixes, noise filters, extra packages
to protect, AppImage directories, the AUR helper, colours and keys.

## Building

```sh
cargo build --release
cargo test          # no system access, no fixtures needed
```

Requires `pacman`. Optional: `snapper` for snapshots, `paru` or `yay` for the
AUR, `flatpak` for Flatpak applications. Each degrades to a stated absence rather
than an error.

## Installing

Once it is on the AUR:

```sh
paru -S apothiki        # or: yay -S apothiki
```

From a checkout, without an AUR helper:

```sh
cd packaging && makepkg -si
```

Or just the binary, no package manager involved:

```sh
cargo build --release
install -Dm755 target/release/apo ~/.local/bin/apo
```

`~/.local/bin` is already on `PATH` on most Arch setups. Nothing else needs
installing — configuration and caches are created on first use.

Publishing a release is documented in [packaging/README.md](packaging/README.md).

## Licence

MIT. See [LICENSE](LICENSE).
