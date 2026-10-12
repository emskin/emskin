# Changelog

All notable changes to emskin are documented here.
Generated from conventional commits via git-cliff.

## [0.3.13] - 2026-10-12

### Bug Fixes
- Keep Emacs child frames above embedded apps (#79)
- Inherit host DISPLAY when satellite is unavailable
- Spawn parked child even when satellite is missing

### Features
- --dbus-isolated mode with private dbus-daemon (#70)
- Floating-dialog routing for embedded toplevels (#69)
## [0.3.12] - 2026-04-27

### Bug Fixes
- Re-evaluate host IME after every tick to catch late text_input bind (#62)
- Reclaim stale X11 display locks on startup
- Set app_id / WM_CLASS to emskin

### Features
- In-process fcitx5 broker + ImeOwner state machine (#68)

### Refactor
- Extract IME/workspace substructs + reorganize into state/ (#59)
## [0.3.11] - 2026-04-21

### Bug Fixes
- Promote xwayland-satellite from optdepends to depends
- Use Window::toplevel() to avoid non-exhaustive match under workspace clippy
- Keep Emacs at bottom of Space to stop app white-screen on resize

### Documentation
- Require a fontconfig-visible font (ttf-dejavu) at runtime
- Update CLAUDE.md + AUR optdepends for satellite backend

### Features
- CLI flag + EmskinState wiring for satellite backend
- Niri-style xwayland-satellite integration components

### Refactor
- Address review feedback + CLAUDE.md bindeps note
- Drop smithay xwayland feature + residual X11 plumbing
- Delete smithay X11Wm path, make satellite the only backend

### Tests
- Widen IPC read timeout + surface child stderr/log on spawn failure
- Drop internal-X ('ix') role from clipboard matrix
- E2E harness + smoke test for satellite backend
## [0.3.10] - 2026-04-20

### Features
- Emez clipboard manager + xdg_activation focus path
- Wl_data_device fallback for KDE/GNOME hosts

### Refactor
- Extract emskin-clipboard as smithay-free crate
## [0.3.9] - 2026-04-19

### Bug Fixes
- Regex-replace emskin literal version via cargo-release
- Bump emskin literal + join workspace shared-version group
- Drain write buffer synchronously on EAGAIN
- Bump emskin crate version to match workspace 0.3.6

### Documentation
- Refresh CLAUDE.md / CONTRIBUTING / READMEs / SKILL for E2E workflow

### Features
- Anvil-pattern X↔Wayland clipboard bridge + test-mode knobs
- Add --version flag with build-time git SHA
- Screencast-style key chord overlay

### Tests
- E2E harness with NestedHost + DisplaySlot + 22-test matrix
## [0.3.4] - 2026-04-18

### Documentation
- Record KeyboardFocusTarget refactor + X11 gotchas

### Refactor
- Unify Wayland/X11 focus via KeyboardFocusTarget
## [0.3.3] - 2026-04-18

### Bug Fixes
- Scope cargo-release hook + drop redundant replacements
## [0.3.0] - 2026-04-18

### Documentation
- Capture-path gotchas + effect-plugins recorder/bitmap_font entries

### Features
- Support pointer-constraints + relative-pointer (#24)
- Screen recording (toggle-record) + screenshot (take_screenshot)
## [0.2.0] - 2026-04-17

### Features
- Jelly text-cursor + unified effect toggle macros (#32)
## [0.1.8] - 2026-04-17

### Bug Fixes
- Use root package so all commits count
## [0.1.6] - 2026-04-17

### Bug Fixes
- Kill bar child on exit to prevent deadlock

### Features
- Cursor trail effect (#28)
## [0.1.5] - 2026-04-16

