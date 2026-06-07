# spacezed

A Spacemacs-flavored fork of [Zed](https://github.com/zed-industries/zed),
built as a cockpit for directing Claude (and other terminal coding agents).

This is one person's opinionated build, shared to **copy as-is**, not to
configure. There are few settings on purpose. If you like it, fork it and
make it yours.

> The default branch (`feat/spacezed`) is the fork. `main` tracks upstream Zed.

## What it adds

- **Spaceline status bar** -- a true Spacemacs powerline: window-number
  badge, per-mode colored state block (NORMAL/INSERT/VISUAL/...), buffer
  name, git branch, and a right side with language, cursor position, and
  scroll percent, joined by GPUI-drawn powerline arrows. No patched font
  needed.
- **which-key as a bottom sheet** -- pressing the `SPC` leader opens a
  full-width sheet docked above the status bar with a column-major
  key -> action grid, instead of a floating popup. It stays open while you
  read (60s), like Spacemacs.
- **Agent cockpit** -- a native, app-global view of every coding agent
  running in a terminal:
  - a cross-window status segment in the spaceline (blocked / working /
    idle counts)
  - `SPC a a` jumps to the agent most deserving of attention (longest
    blocked, else longest idle), across all windows
  - `SPC a l` opens an urgency-sorted agent picker
  - `SPC a n` spawns a proper center-tab agent terminal
  - status comes from Claude Code hooks (see `script/spacezed-setup-hooks`),
    keyed to each terminal by an injected `SPACEZED_TERM_ID`
- **Terminal vi mode that respects the leader** -- in terminal vi (command)
  mode, keys the mode does not handle propagate to the keymap instead of
  being swallowed, so the `SPC` leader and which-key work inside a terminal
  pane. (Upstream swallows everything.)
- **`project_environment` setting** -- `all | top_level | root_only | off`
  to control Zed's per-git-repo environment probe, which is pathologically
  slow on umbrella folders full of submodules.

## Build it yourself

Requires the Rust toolchain via [rustup](https://rustup.rs) and, on macOS,
Xcode with the Metal toolchain (`xcodebuild -downloadComponent
MetalToolchain` on Xcode 26+).

```sh
git clone https://github.com/raouf2ouf/spacezed
cd spacezed                      # default branch is feat/spacezed
script/bundle-mac                # macOS app bundle
# then move target/.../dmg/Zed.app to ~/Applications
```

To wire up agent status, run `script/spacezed-setup-hooks` once (it installs
the hooks idempotently into `~/.claude/settings.json`, backing it up first).

The leader keymap and Spacemacs Dark theme are user config, not part of the
fork; they live in `~/.config/zed/{keymap.json,settings.json,themes/}`.

## Relationship to upstream

Some commits here are general-purpose and may be proposed upstream
(`project_environment`, the terminal vi-mode key fix). The rest are
deliberately opinionated and live only on the fork. `main` stays in sync
with `zed-industries/zed` so the fork can be rebased onto new releases.

Zed is licensed GPL-3.0; so is this fork.
