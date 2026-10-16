# hashterm 0.3 / 0.4 implementation plan

Status: **proposal, nothing implemented.** Written 2026-10-15 against `main` at `d9597c6` (v0.2.2).
Scope: the ten items from the product review. Items 1-5 fix the first minute (release 0.3.0); items 6-10 close the
table-stakes gaps (release 0.4.0).

Evidence tags used throughout:

- **[V]** verified this session by a spike or measurement (Appendix A has the numbers and recipes)
- **[R]** read directly in the code or docs at the cited `file:line` (anchors are for `d9597c6`)
- **[I]** inference, not tested
- **[E]** effort estimate, focused engineering days for one developer

Every [V] figure was re-measured or re-read on 2026-10-15 immediately before the final edit (Appendix A lists what was
run). The spike scripts that produced them are not part of this repository.

## 1. Summary

| # | Item | Release | Effort | Depends on | Main files |
|---|------|---------|--------|------------|------------|
| 0+5 | Format + lint cleanup, CI that runs tests | 0.3.0 | 1.0 d | none (do first) | `.github/workflows/ci.yml`, `rust-toolchain.toml` |
| 3 | Hotkey status, log file, `--doctor`, first-run hint | 0.3.0 | 2.5 d | F3, F5 | `hotkey.rs`, `core/lib.rs`, `main.rs`, `settings.rs` |
| 1 | Tab and group chrome that works in any theme | 0.3.0 | 1.5 d | F1 | `chrome.rs` (new), `css.rs`, `style.css`, `tabbar.rs`, `groupbar.rs` |
| 2 | Tab titles that tell tabs apart | 0.3.0 | 1.5 d | F6 | `title.rs` (new), `controller.rs`, `window.rs` |
| 4 | Make the matrix discoverable | 0.3.0 | 1.0 d | F2, F5 | `tabbar.rs`, `window.rs`, `config.rs` |
| - | Screenshots, docs, rc (incl. the `~rc` deb version fix, section 9), release | 0.3.0 | 1.25 d | all above | `README.md`, `docs/`, `packaging/build-deb.sh` |
| 8 | Colour schemes, font zoom, cursor | 0.4.0 | 3.5 d | F1 | `core/scheme.rs` (new), `page.rs`, `css.rs`, `config.rs` |
| 6 | Scrollback search | 0.4.0 | 3.0 d | F6, 8 (styles) | `search.rs` (new), `controller.rs`, `window.rs` |
| 7 | Clickable links | 0.4.0 | 2.0 d (+1.0 for file paths) | tmux conf | `page.rs`, `links.rs` (new), `hashterm.tmux.conf` |
| 9 | Notifications and attention badges | 0.4.0 | 2.5 d | F1 | `window.rs`, `tabbar.rs`, `groupbar.rs`, `app.rs` |
| 10 | Splits as tmux panes | 0.4.0 | 2.5 d | F2, F6, 8 | `window.rs`, `controller.rs`, `config.rs` |
| - | Screenshots, docs, rc, release | 0.4.0 | 1.5 d | all above | `README.md`, `docs/` |

Totals: about 9 days for 0.3.0 and about 16 days for 0.4.0 (including file paths). The review's first estimate was
4-5 and 8-12 days. It rose because this plan includes tests, docs, screenshots and the shared foundations (section 4),
which the first estimate left out.

## 2. Ground rules

1. **Expose tmux, don't rebuild it.** Search, copy mode, panes and prompt navigation already exist in tmux. The GUI
   adds keys, a find bar and styling on top.
2. **One owner for colours.** Chrome colours are derived from the active scheme in one module (F1). No hard-coded chrome
   hex values anywhere else.
3. **Every new knob is complete.** A `config.rs` field with a serde default, the commented key in
   `assets/default-config.toml`, an `f!` row in `crates/hashterm-ui/src/settings.rs`, a README/docs line, and the
   existing `shipped_default_config_matches_code_defaults` test staying green.
4. **Never block the GTK thread on tmux.** One small tmux call costs about 4 ms (median 3.8, p95 4.6, max 7.1) and the
   pane-facts call stays near 5-6 ms up to ~100 panes [V]. A search takes 50-131 ms on a 50k-line history [V]. Anything
   repeated or slow runs off-thread via `gtk4::gio::spawn_blocking` (present in the vendored gio 0.22 [V], no new
   dependency).
5. **No new crates unless already in the offline registry.** `tracing-appender` and `shell-words` are not there [V], so
   log rotation and argument splitting use std. Builds are `--offline`.
6. **Every item ships with an observable acceptance check** that can run headless (private tmux server, Xvnc, xdotool).
7. **Existing contracts hold.** The tab-close contract (previous tab, or next if first; empty-group prompt) is unchanged
   for single-pane tabs. See decision D1 for the one place splits touch it.

## 3. What the spikes changed from the first draft

| First draft | Measured | Plan now |
|---|---|---|
| Titles: set a tmux `set-titles-string` format | With `status off` the title follows only the first `cd` (after 3.5-3.8 s). The next three `cd`s and a foreground program name never arrived in 20 s each, with or without `status-interval` [V] | GUI derives titles from pane facts it polls (item 2) |
| Pin the dark GTK variant | Token CSS renders identically under light and dark GTK themes [V] | Do not pin; chrome is independent of the GTK theme (D3) |
| Activity badge from tmux `alert-activity` | The hook never fires for attached current windows; `alert-silence` neither [V]. Only `alert-bell` fires [V] | Activity comes from VTE `contents-changed`, which fires for hidden tabs and a hidden window [V] (item 9) |
| Links: enable VTE matching | tmux drops OSC 8 hyperlinks unless `terminal-features` has `hyperlinks` for the outer TERM [V] | tmux conf change is part of item 7 |
| Search: use VTE search | VTE history is empty (tmux owns it; alternate screen [V]; `scrollback_lines(0)` at `page.rs:25` [R]) | Drive tmux copy-mode (item 6) |
| Capability probe for tmux versions | `search_count` arrived in tmux 3.5 (CHANGES); Debian 13 ships 3.5a, Ubuntu 25.04 3.5a, 26.04 3.6a [V] | Declare tmux >= 3.5 in `Depends` and README instead of gating code |
| 4-5 and 8-12 days | See section 1 | 9 and 16 days |

## 4. Foundations (built once, consumed by several items)

**F1. Chrome tokens** (`crates/hashterm-ui/src/chrome.rs`, built in item 1). `ChromeTokens::derive(bg, fg)` and
`chrome_css(&tokens) -> String`, installed by `Styles::apply` into the generated layer (`css.rs:37-72`, priority
APPLICATION+1). Consumers: items 1, 6 (search bar), 8, 9 (badge colours), 10 (pane borders).

**F2. One action table** (`crates/hashterm-ui/src/app.rs:413-495`). Today an action exists in three parallel places:
`register_actions` (a fixed-size array, `[...; 13]` at `app.rs:414`), `apply_accels` (a second list, `app.rs:449-462`)
and the Settings Keyboard rows (`settings.rs:519-635`). The plan adds 12 hotkeys, so introduce one
`const ACTIONS: &[ActionDef]` (name, accessor into `Hotkeys`, handler) that both functions iterate. Add a test that
every `Hotkeys` field (14 today, 26 after this plan) has an action, a default accelerator and a Settings row (serialize
`Hotkeys::default()` and compare its keys against `PAGES`; `serde_json` is a dependency of the core, session, tmux and
ctl crates but not of `hashterm-ui`, so add it there as a dev-dependency, it is already in the lockfile). `ActionDef`
also carries optional extra accelerators (paste already binds `<Shift>Insert`, `app.rs:489`); zoom needs that for
`<Ctrl>equal` and `<Ctrl>KP_Add` next to `<Ctrl>plus`. Conflict detection (`app.rs:463-485`) then covers new keys for
free.

**F3. Logging** (`crates/hashterm-core/src/lib.rs:37-47`, built in item 3). File sink plus panic hook, used to debug
every later item.

**F5. Notice surface** (`crates/hashterm-ui/src/notice.rs`, built in item 3). A dismissible, actionable strip overlaid at
the top of the console, below the tab bar, never taking focus. Persistent "seen" flags in `UiState`. Consumers: items 3
and 4. It complements the transient `show_osd_message` (`window.rs:1587`), which has no buttons.

**F6. Off-thread tmux and pane facts** (`controller.rs`, `window.rs`, built in item 2). `TmuxController` is `Clone`
(`controller.rs:63`), so it moves into `spawn_blocking`. One call, `list-panes -a -F` with
`session_name, window_active, pane_active, pane_id, pane_current_path, pane_current_command, window_panes, window_zoomed_flag`,
returns every fact that items 2, 6 and 10 need. Results are applied on the main thread with a generation counter so
stale replies are dropped.

(F4, the tmux capability probe, was dropped: see section 3.)

## 5. Order and dependencies

```
Step 0 fmt/clippy ─► 5 CI ─► 3a logging ─► F5 notice ─► 1 chrome (F1) ─► 2 titles (F6) ─► 3b/c/d ─► 4 ─► release 0.3.0
                                                            │                │
                                                            ▼                ▼
0.4.0:   8 schemes (uses F1) ─► 6 search (uses F6, tmux styles from 8) ─► 7 links ─► 9 notifications (uses F1) ─► 10 splits (uses F2, F6, 8)
```

Within 0.4.0 the order is flexible except that 10 needs 8's tmux-style sync (pane borders) and 6 and 10 both need F6.

## 6. Items

### Step 0 and item 5: tests, lint and format in CI (0.3.0, 1.0 d [E])

**Outcome.** Every push runs fmt, clippy and the tests, so the ten items land on a net.

**Baseline [V].**
- `.github/workflows/` holds only `slsa-release.yml`; nothing runs on push.
- `cargo test --workspace --offline --no-fail-fast`: 67 tests pass in 3 s (incremental) with no display, including the
  tmux end-to-end `crates/hashterm/tests/resurrect.rs`. It needs tmux installed and a **short** `TMUX_TMPDIR`: under a
  deep scratch path the unix socket name exceeded 108 bytes and the test failed with "File name too long" (our harness,
  not the test).
- That test prints "tmux not installed; skipping" and passes when tmux is missing (`resurrect.rs:30-33` [R]), so CI must
  install tmux and assert `tmux -V`, or a green run can mean "skipped".
- `cargo clippy --workspace --all-targets` (1.96.0): 0 errors, 5 warnings: `settings.rs:754:25` (very complex type),
  `settings.rs:872:49` (needless borrow), `examples/title_leak_repro.rs:39:13` (deprecated `show()`), `:93:12` and `:113:12`
  (`is_multiple_of`).
- `rust-toolchain.toml` pins 1.96.0 with `components = ["clippy"]` only, so `cargo fmt` does not run under the pin.
  Measured with the default toolchain's rustfmt 1.9.0 (2026-09-01, edition 2024; the pinned 1.96.0 ships no rustfmt, so
  recount under the pin after adding the component): **13 of 36 tracked files differ, 73 hunks**.

**Changes.**
1. Commit A, `style: rustfmt`: mechanical, 13 files. Add its hash to `.git-blame-ignore-revs`. Add `rustfmt` to
   `components` in `rust-toolchain.toml`.
2. Commit B, `lint: clear clippy warnings`: the five above (`type` alias for the complex type, drop the borrow,
   `set_visible(true)`, `is_multiple_of`).
3. New `.github/workflows/ci.yml`:
   - Triggers: `push` to `main` and `pull_request`; `permissions: contents: read`; `concurrency` cancel-in-progress.
   - Runs in the same `debian:trixie` container as the release workflow, with the same apt list
     (`slsa-release.yml:63-69`) plus `tmux`, and the same Rust bootstrap (`slsa-release.yml:71-76`). Do not invent a second
     install method; when issue #43 (pin rustup-init) is resolved, change both files together.
   - Steps: `tmux -V`; `cargo fmt --all --check`; `cargo clippy --workspace --all-targets --locked -- -D warnings`;
     `cargo test --workspace --locked --no-fail-fast` with `TMUX_TMPDIR=$(mktemp -d /tmp/htci.XXXXXX)`,
     `HASHTERM_SOCKET=ci`, private `XDG_*`, and no `DISPLAY`.
   - Pin new `actions/*` usages by commit SHA from the start (issue #44 tracks the older ones).
   - `cargo deny check advisories bans sources licenses` as a separate job on a weekly `schedule` and on `main`, to keep
     push latency down (the release workflow already runs it).
   - Cache `~/.cargo/registry` and `target` keyed on `Cargo.lock` plus the toolchain file.
4. README: CI badge. `docs/RELEASING.md`: one line, "CI must be green on the tagged commit".

**Acceptance.** A PR with a deliberate fmt error, a clippy warning, and a failing test each turns CI red on a throwaway
branch (needs a push, the owner's call); a clean PR is green; the tmux assertion fails the job if tmux is missing.

**Risks.** New clippy lints on toolchain bumps (mitigated: the toolchain is pinned). CI minutes (mitigated: cache,
deny on schedule).

### Item 3: hotkey status, log file, `--doctor`, first-run hint (0.3.0, 2.5 d [E])

**Outcome.** A failed global hotkey is visible and fixable; bug reports carry logs; a new user learns the entry points.

**Current state [R/V].**
- `register_toggle_hotkey` returns an optimistic `HotkeySourceKind::Portal` (`hotkey.rs:23-39`). The real portal result
  arrives later on a thread (`spawn_portal_bind`, `hotkey.rs:86-120`), which then falls back to an X11 grab or logs.
- `wire_global_hotkey` only logs (`app.rs:271-297`). `init_logging` writes to stderr only (`core/lib.rs:37-47`); the live
  process has stdout and stderr on `/dev/null` and `~/.local/state/hashterm` does not exist [V], so failures and panics
  leave no trace.
- `bind_shortcuts` returns `trigger_description` per shortcut but it is only logged (`hotkey.rs:217-227`).
- On GNOME Wayland an X11 key grab can hide a window but not show it (guake#2350, 2026-09-16, text read [V]).
  hashterm's fallback is the same mechanism but was not reproduced here [I].

**Design.**
- **3a Logging.** `init_logging()` keeps stderr (when it is a TTY) and adds a file sink at
  `$XDG_STATE_HOME/hashterm/hashterm.log` (dir 0700, file 0600). Rotation at startup: if the file exceeds 2 MiB, rename
  to `hashterm.log.1`, keeping one backup. Implement with std and a `MakeWriter` over `Mutex<File>` (no
  `tracing-appender`). Add a panic hook that logs message, location and `Backtrace::force_capture()` before delegating
  to the previous hook. `HASHTERM_LOG` still wins. `restore-pane` stays logging-free (`main.rs:113-134`). Knob:
  `[general] log_to_file = true`, with a Settings row "Keep a log file for bug reports". Never log terminal content.
- **3b Hotkey status.**
  `enum HotkeyStatus { Pending, Active { source, trigger }, Degraded { reason }, Unavailable { reason } }` delivered
  through a callback added to `register_toggle_hotkey`. `PortalMsg` gains `Bound(trigger_description)` and
  `Failed(reason)`. An X11 grab in a Wayland session (`XDG_SESSION_TYPE=wayland` and XWayland) is `Degraded` ("works
  only while an X11 window has focus"). Surfaces: (i) a read-only status row in Settings > General with "Re-register"
  and "Copy fallback command" buttons (new `Kind::Status`), (ii) a one-time Notice for `Degraded` and `Unavailable`
  offering "Copy `hashterm --toggle`" and "Don't remind me", (iii) a warn log line. While `Pending`, wait 20 s before
  showing anything (the GNOME consent dialog may be open).
- **3c `hashterm --doctor`.** Handled before GTK init like `--restart` (`main.rs:136-140`). Prints, with `$HOME`
  redacted to `~`: hashterm, GTK, VTE, glib and tmux versions (warn if tmux < 3.5); session type, desktop and GDK
  backend (and whether it was forced); GlobalShortcuts portal presence and version via `ashpd` (a probe, no bind);
  config path and parse result; state and log paths and sizes; tmux socket and session count; clipboard tool;
  `xdg-open`. `--log` appends the last 40 log lines. Exit code 1 if any check fails.
- **3d First-run hint.** `UiState.hints_seen: Vec<String>` (`state.rs:19-37`). On the first present, a Notice:
  "F12 shows and hides this terminal · Ctrl+Shift+T new tab · right-click a tab for groups and colours · Ctrl+, settings
  [Got it]". Omit the F12 part when `dropdown.enabled = false`. Dismissal persists.
- **F5 Notice** is built here: `Revealer` + `Box`, `can_focus(false)`, `AccessibleRole::Alert`, styled from F1 tokens.

**Edge cases.** Portal denied by the user; no D-Bus session; `HASHTERM_NO_PORTAL=1` (exists, `hotkey.rs:44`); log dir not
writable (fall back to stderr only, warn once); `--doctor` run while another instance is up.

**Tests.** Unit: rotation with a temp dir; a pure function mapping (portal result, session type) to `HotkeyStatus`;
doctor output sections and `$HOME` redaction; `hints_seen` round trip. Integration: a debug-only `HASHTERM_TEST_PANIC`
that panics after startup and asserts the log contains the message.

**Acceptance.** With `HASHTERM_NO_PORTAL=1` under XWayland: Settings shows "Degraded" and the Notice appears once and
not again after dismissal. A log file exists with the startup lines; `hashterm --doctor` prints every section and exits 0
on a healthy machine. The hint appears on a fresh profile and never steals focus (typing continues in the terminal).

**Risks.** The GNOME consent dialog timing (hence the 20 s `Pending` grace); the Notice covering the first terminal rows
(mitigated: top, small, dismissible).

### Item 1: tab and group chrome that works in any theme (0.3.0, 1.5 d [E])

**Outcome.** Legible, consistent chrome under any GTK theme and any scheme; the active tab is obvious; "+" and "x" are
visible.

**Root cause [V].** The GTK theme paints `background-image` (a gradient or flat image) over our `background-color`, so the
stylesheet's `.tab:checked { background-color }` never shows. Measured under GTK 4.18.6 with the current CSS:

| Element | Light theme | Dark theme |
|---|---|---|
| Active tab fill / label | `#dad6d2` / white, **1.45:1** | `#232323` (stylesheet says `#2b2f33`) / white, 15.7:1 |
| Inactive tab label | **1.86:1** | 5.69:1 |
| Tab-bar "+" | **1.41:1** | 15.3:1 |
| Group-bar "+" and "x" (not seen in any earlier review) | **1.48:1** | 16.2:1 |
| Group row label, rest | **2.46:1** | 4.46:1 |

**Current state [R].** `style.css:38-137` (tab, group, badge rules); generated layer `css.rs:49-72` sets only
`window.hashterm`, `.console-body`, `.tabbar` (the terminal background minus 7 per channel) and `.terminal-page`; tab
widgets `tabbar.rs:411-565` (`ToggleButton.tab`; the label has `hexpand(true)` at `tabbar.rs:462`, which makes every tab
stretch to an equal share: 936 px each with two tabs in a 1920 px window [V]); `.tab-close` (`tabbar.rs:468-472`) and
`.tab-new` (`tabbar.rs:567-571`) have no CSS rules at all; group widgets `groupbar.rs:240-483`. Per-tab accents are an
inset `box-shadow` underline from a provider at APPLICATION+2 (`window.rs:313-319`, rule at `window.rs:2860`), so it
outranks the base chrome rules.

**Design.**
1. **`chrome.rs`** (F1): `ChromeTokens::derive(bg, fg)`. Derivation, validated on 14 representative palettes:
   ```
   dark     = luminance(bg) < 0.4
   strip    = mix(bg, black, dark ? 0.22 : 0.07)
   groupbar = mix(bg, black, dark ? 0.42 : 0.12)
   fill_mix = smallest m in 0.10..0.45 (step 0.01) with contrast(mix(strip, fg, m), strip) >= (dark ? 1.30 : 1.15)
   active   = mix(strip, fg, fill_mix);  hover = mix(strip, fg, min(fill_mix * 0.5, 0.20))
   muted    = ensure(mix(strip, fg, 0.72), against [strip, hover, active], 4.5)
   text_active = ensure(mix(fg, white_or_black, 0.35), against [active], 7.0)
   group_muted = ensure(mix(groupbar, fg, 0.72), against [groupbar, mix(groupbar, fg, 0.10)], 4.5)
   ensure(c, backgrounds, target): push c toward white (dark) or black (light) in 2% steps until every contrast >= target
   ```
   Worst case over the 14 palettes [V]: muted label 5.23:1 on the strip, 4.87:1 on hover and 4.52:1 on the active fill;
   active label 7.04:1; active fill vs strip 1.30 (dark) and 1.16 (light); group label 5.04:1. Badge colours
   (`.tab.bell/.activity/.external`) are derived the same way so they stay legible on light chrome.
2. **Rule set** (generated, Appendix B), proven in the GTK spike. Every state of `.tab`, `.group-row`, `.tab-new`,
   `.tab-close`, `.group-new` and `.group-close` sets `background-image: none`, an explicit background and foreground, and
   leaves GTK's focus ring alone. No font-weight change (it would resize tabs on every switch), no backdrop dimming (it
   reads as "disabled").
3. **`style.css`** keeps layout only (padding, radius, OSD, settings, `.group-close` geometry); chrome colours move out so
   there is one owner. `Styles::apply` generates them from the effective scheme.
4. **Tab sizing.** Label `hexpand(false)`, `max_width_chars(24)`, `width_chars(6)`; tab `hexpand(false)`, `halign(Start)`,
   so "+" sits right after the last tab. Side bars keep their fixed 180 px.
5. **Tooltips with shortcuts.** "+" "New tab (Ctrl+Shift+T)", "x" "Close tab (Ctrl+Shift+W)", the tab shows its full title.
   Accelerator text comes from the live `Hotkeys` via `gtk4::accelerator_get_label`, so rebinding updates tooltips. The main
   window has two tooltips today [V]; only the group buttons have any.
6. **User CSS.** `~/.config/hashterm/user.css` loads at USER priority (`css.rs:26` [R]). The theme's `background-image` also
   hides a user's `background-color` on tabs, so such overrides should start working once the chrome rules set
   `background-image: none` [I, not tested]; say so in the README.

**Tests.** Unit (`chrome.rs`): for every palette fixture assert the thresholds above, plus hex round trip and the `ensure`
termination. GTK spike promoted to a dev script (kept outside the repo until it stabilises) that renders every state under
`GTK_THEME` unset and `Adwaita:dark` and measures contrast from pixels.

**Acceptance (measured, both GTK variants, default scheme).** Every label >= 4.5:1 at rest and on hover, >= 7:1 when active;
icons >= 4.5:1; active fill vs strip >= 1.30:1; the two-tab strip leaves tabs under 300 px at 1920 px wide; tooltips
present; README images re-captured.

**Risks.** GTK minor-version CSS differences (the floor is 4.18; re-run the matrix on 4.22 when an Ubuntu 26.04 or
Fedora 44 machine is available). Very low-contrast schemes are handled by the clamps.

### Item 2: tab titles that tell tabs apart (0.3.0, 1.5 d [E])

**Outcome.** New tabs are labelled by folder (or the running program) instead of the hostname; titles a program sets win
immediately.

**Current state [R/V].** The label comes from VTE's `xterm.title` termprop (`page.rs:43-59`), fed by tmux
`set-titles-string "#{pane_title}"`. `pane_title` defaults to the host name [V]. The skeleton `~/.bashrc` sets a title
only when `TERM` matches `xterm*|rxvt*` (`/etc/skel/.bashrc:66-68` [R]) while `TERM` inside hashterm's tmux is
`tmux-256color` (`hashterm.tmux.conf:5`), so a stock bash never sets one and every default tab shows the host name
(other shells and prompt frameworks vary [I]). A new tab is first labelled "shell" (`window.rs:641`), then replaced by
the hostname at attach.

**Spike findings [V].** A program-set title (OSC 2) reaches the client in 0.05 s and survives a later `cd`. The default
`pane_title` is the host name, which is why every new tab shows it. A tmux-side format (a conditional that
falls back to the folder name when the title equals the host) is not an option: with `status off` the title followed the
first `cd` after 3.5-3.8 s, and neither the next three `cd`s nor a foreground program name ever arrived (20 s wait each),
with or without `status-interval`; with `status on` and interval 1 it follows in 0.8 s, but a status row is unacceptable.
The facts the GUI needs are available from tmux: `pane_current_path` and `pane_current_command` followed a `cd` and a
running `sleep`, and went back to `bash` after Ctrl-C (0.5 s settle).

**Design.** A pure function in `crates/hashterm-ui/src/title.rs`:
```
auto_title(vte_title, host, host_short, cwd, cmd, home) -> String
  if vte_title is non-empty and not equal (case-insensitive) to host or host_short: return vte_title
  base = "~" if cwd == home, "/" if cwd == "/", else basename(cwd)
  if cmd is not a shell (bash zsh fish sh dash ksh csh tcsh ash, or the $SHELL basename): "{cmd} · {base}" else base
  strip control chars, cap at 64
```
- **Facts** come from F6: one `list-panes -a -F` per tick, active pane per session. A 2 s timer runs only while the
  window is mapped and visible, and is off while hidden (0 calls). One call takes a median of 3.5 ms with 4 panes, 5.2
  ms with 50 sessions and 5.7 ms with ~100 panes [V]; it runs off-thread. Also refresh on tab creation and selection.
- `set_tab_title` already skips unchanged titles (`window.rs:2398`), so a steady state causes no widget rebuilds, which also
  respects the GTK redraw leak noted at `main.rs:88-94`.
- A user-renamed tab (`title_custom`) always wins (`window.rs:2395`).
- Knob: `[tabbar] auto_titles = true` (Settings row "Name tabs after the folder or running program"); off keeps today's
  behaviour.

**Tests.** Unit: the function over (hostname equality in both cases, `~`, `/`, shells, `ssh`, `vim`, non-UTF paths,
control characters). Integration on a scratch tmux server: the facts call returns the expected cwd and command after a
`cd` and a `sleep`.

**Acceptance.** A new tab shows its folder within 2 s on a profile with no prompt customisation; after `cd` the label
follows within 2.5 s; `vim file` shows "vim · folder"; a program OSC 2 title shows at once; a renamed tab is untouched;
with the window hidden, a 60 s debug log shows no title-poll tmux calls.

**Risks.** Polling cost grows with pane count (median 3.5 ms at 4 panes, 5.2 ms at 51, 5.7 ms at 101; p95 7.6 ms [V]);
re-measure inside the app at 50 tabs.

### Item 4: make the matrix discoverable (0.3.0, 1.0 d [E])

**Outcome.** A day-one user can find groups without reading the README.

**Current state [R].** The group bar is hidden with a single group (`window.rs:2828-2830`, `hide_when_single = true` at
`config.rs:189`); creating a group needs the group bar's "+" (`groupbar.rs:439-452`) or the tab menu, and there is no
shortcut for it (`config.rs:305-323`).

**Design.**
- When the group bar is hidden, show a compact "+ Group" chip at the end of the tab strip (`TabBar` gets a
  `show_group_chip` flag; styled like `.tab-new` with text). It emits a new `TabBarEvent::NewGroup`, handled by the
  existing `new_group_with_tab` (`window.rs:2130`; the group bar's `GroupBarEvent::New` already calls it,
  `window.rs:2564`). No `new_group` hotkey exists today [V]. The chip disappears when the bar appears.
- New hotkey `new_group = "<Ctrl><Shift>n"`, shown in the chip's tooltip; Settings and default-config rows via F2.
- The first-run Notice (item 3d) mentions groups.
- Decision D2: keep `hide_when_single = true`. A permanent single-row bar costs space for people who never use groups.

**Tests.** Pure `show_group_chip(cfg, group_count)`; headless: click the chip, assert two groups via
`tmux list-sessions -F '#{@hashterm_group}'`.

**Acceptance.** A fresh profile shows the chip; clicking it yields a second group, the bar reveals with two rows and "+",
and the chip disappears; Ctrl+Shift+N does the same.

### Item 8: colour schemes, font zoom, cursor (0.4.0, 3.5 d [E])

**Outcome.** Pick a scheme in Settings and everything follows live: terminal, ANSI palette, cursor, selection, chrome, tmux
highlights and pane borders. Zoom with Ctrl+plus/minus/0. Optionally follow the system light/dark preference.

**Current state [R].** `Appearance` (`config.rs:100-119`, `deny_unknown_fields`) has font, padding, three opacity
values, `tab_border_width`, `foreground` and `background` (plain strings) and `bold_is_bright`, with no palette, cursor
or scheme; `apply_colors` passes an empty palette (`page.rs:246-253`, so ANSI colours are VTE's built-in set); the tab
strip colour is derived from the background (`css.rs:53-59`); a config change re-applies appearance to every live page
(`window.rs:2436-2440`).

**Design.**
- **Scheme model** (`crates/hashterm-core/src/scheme.rs`, GTK-free):
  `{ name, dark, fg, bg, cursor, selection_bg, selection_fg, palette[16], accent }`, embedded from
  `assets/schemes/*.toml` via `include_str!`, plus user schemes from `~/.config/hashterm/schemes/*.toml` (data only).
  About 12 bundled: the current look as `default` (empty palette, so nothing changes for anyone who does not opt in)
  plus well-known dark and light schemes. **Verify each upstream licence at bundling time** and record provenance in
  `assets/schemes/README.md`; that is not done here.
- **Config.** `[appearance] scheme = "default"`, `follow_system = false`, `scheme_dark`, `scheme_light`,
  `cursor_shape = "block"|"ibeam"|"underline"`, `cursor_blink = "system"|"on"|"off"`. `foreground` and `background`
  become `Option<String>` explicit overrides (old files stay valid; a set key still wins). The Settings colour rows show
  the scheme's value when unset and write the key on edit; reset removes it. Update
  `shipped_default_config_matches_code_defaults`.
- **Apply.** `apply_colors` gains the palette, cursor, highlight colours, `set_cursor_shape`, `set_cursor_blink_mode`.
  Chrome tokens derive from the effective bg/fg (F1). `TmuxStyles::apply(ctl, &scheme)` sets `mode-style`,
  `copy-mode-match-style`, `copy-mode-current-match-style`, `pane-border-style`, `pane-active-border-style` with
  `set-option -g`, idempotently, on scheme change and when adopting a server. Those options exist on 3.5a [V].
- **Follow system.** `ashpd` `Settings::color_scheme()` plus `receive_color_scheme_changed()`. `ashpd` 0.13.13 is
  already in the lockfile and has a dependency-free `settings` feature [V]; enable it next to `async-io` and
  `global_shortcuts` in the workspace `Cargo.toml`.
- **Zoom.** Actions `zoom-in/out/reset` (F2; zoom-in also takes `<Ctrl>equal` and `<Ctrl>KP_Add`, because `+` needs
  Shift on US layouts) using `Terminal::set_font_scale`, clamped 0.5-3.0 in 0.1 steps, remembered in
  `UiState.font_scale`, shown as an OSD percentage. Ctrl+wheel in the terminal is optional (capture-phase scroll
  controller, only when Ctrl is held). A scale change resizes the tmux client; `window-size latest` and the existing
  `wire_redraw_on_resize` cover it.
- **Settings.** Appearance gets a scheme dropdown (a plain `Kind::Choice` first; swatch preview is polish), cursor rows, and
  follow-system rows.

**Tests.** Every bundled file parses, has 16 colours, unique names; chrome thresholds hold for every bundled scheme
(extends item 1's test); config round trip and defaults; zoom clamp. Headless: change `scheme` in the file and compare the
terminal's pixel colour before/after; `tmux show -g pane-border-style` equals the expected value.

**Acceptance.** A scheme switch updates all surfaces live; the default scheme is pixel-identical to 0.2.2; follow-system
flips with `gsettings set org.gnome.desktop.interface color-scheme prefer-light` within 1 s; zoom persists across restarts.

**Risks.** Licence provenance; changing `foreground/background` to `Option` touches the Settings colour rows; light schemes
exercise the chrome clamps (covered by item 1's test).

### Item 6: scrollback search (0.4.0, 3.0 d [E])

**Outcome.** Ctrl+Shift+F opens a find bar; typing highlights matches; Enter and Shift+Enter move; Esc closes.

**Facts [V].** VTE's history is empty (alternate screen; `page.rs:25`), so VTE search finds nothing. tmux copy-mode does the
work: `copy-mode -t PANE`, `send-keys -t PANE -X search-backward-text TEXT`, `search-again`, `search-reverse`, `cancel`.
The tmux binary exposes exactly four search formats, `#{search_present} #{search_count} #{search_count_partial}
#{search_match}` (count needs tmux >= 3.5), so the count is a total only: there is no "current match index". On a
49,973-line history a search took 131 ms for one match deep in the history, 91 ms for 1,111 matches, 65 ms for 50,001
matches and 50 ms for none; reading the count afterwards took 4-5 ms. Two behaviours the UI must respect: the search is
smart-case (all-lowercase input also matches capitals; any capital makes it case-sensitive), and after a miss in the same
copy-mode session `search_count` keeps its previous value (read `search_present` first; in a fresh copy-mode session a miss
gives present=0, count=0). `copy-mode-match-style` and `copy-mode-current-match-style` exist.

**Design.**
- **Widget** (`search.rs`): a `Revealer` overlay at the top right (an overlay child like the OSD, `window.rs:239-242`)
  with `SearchEntry`, previous/next buttons, a count label ("1,111 matches" / "No matches") and close. Ctrl+Shift+F opens
  and focuses; Enter = `search-again`, Shift+Enter = `search-reverse`, Esc = `cancel` and refocus the terminal. Tab switch
  closes it and cancels on the old pane.
- **Controller.** Pure argument builders on `TmuxController`: `search_enter`, `search_text`, `search_again`,
  `search_info`, `search_cancel`, all targeting the active pane id from F6. Execution is off-thread, single-flight, with
  a 150 ms debounce and a generation counter that drops stale results. Each new query is `cancel`, `copy-mode`, then
  `search-backward-text`, so the search always starts from the bottom and the count is never stale. The result label
  reads `search_present` first and shows "No matches" without looking at the count.
- **Styles** come from item 8's `TmuxStyles` (amber default until then).
- **Knob:** hotkey `search = "<Ctrl><Shift>f"` only.

**Not verified [I].** Full-screen programs (vim, less) have no history, so copy-mode should search the visible screen only
(expected from tmux's model, not tested). The find bar's tooltip says "Lowercase ignores case".

**Tests.** Argument-builder unit tests; debounce and generation logic as a pure struct with an injected clock;
scratch-server integration (extend `controller.rs:358`): 5k lines, assert count, `search_present`, smart-case and the
miss-after-hit case; headless: type in the bar, assert `#{pane_in_mode}` and `#{search_count}`, then Esc and
`pane_in_mode = 0`.

**Acceptance.** On a 50k-line history the highlight appears within 400 ms of the last keystroke; no main-thread stall over
50 ms; Esc returns focus to the terminal; works with 1 and 3 panes.

### Item 7: clickable links (0.4.0, 2.0 d + 1.0 d for file paths [E])

**Outcome.** Ctrl+click opens URLs, including OSC 8 hyperlinks, in the default browser.

**Facts [V].** Under tmux mouse mode a Ctrl+click is forwarded to tmux (1 report). A capture-phase `GestureClick` that
claims the sequence only when Ctrl is held blocks it (0 reports) while plain clicks still flow (2 of 2). tmux 3.5a drops
OSC 8 unless the outer terminal has the `hyperlinks` feature. VTE's `allow-hyperlink` is off by default and not enabled
(`page.rs`: no hyperlink or match calls, grep [V]).

**Design (7a, URLs and OSC 8).**
- **tmux conf:** add `set -as terminal-features ',xterm-256color:hyperlinks'` to `assets/hashterm.tmux.conf`, and apply
  it to a running server at startup, idempotently (check `show-options -s terminal-features` first), like the existing
  `copy-command` call (`app.rs:405-407`).
- **Page:** `set_allow_hyperlink(true)`; `match_add_regex` with a URL pattern and `match_set_cursor_name("pointer")`.
  VTE needs the PCRE2 multiline flag in `Regex::for_match` [I: confirm].
- **Interaction:** a capture-phase `GestureClick` (button 1) that, with Ctrl held, resolves the link under the pointer
  (`Terminal::check_hyperlink_at(x, y)` first, then `check_match_at(x, y)` for the regex match; both exist in the locked
  vte4 0.10, the GTK3-era `hyperlink_check_event` does not [V]). If found, claim and open; otherwise do not claim, so
  tmux gets the click. A capture-phase motion controller shows the pointer cursor and a tooltip with the full resolved
  URL while Ctrl is held. Right-click adds "Copy link".
- **Wrapped URLs** [I, not spiked]: tmux repaints with explicit cursor moves, so VTE cannot join a URL wrapped at the pane
  edge. When a match ends at the last column, append the next row's leading non-space run; the tooltip shows what will
  open. Add a wrapped-URL fixture to the tests.
- **Open** with `gtk4::UriLauncher` (already used at `app.rs:504`). **Scheme allowlist:** `http`, `https`, `mailto`.
  Anything else is refused with an OSD naming the scheme. Never auto-open; Ctrl+click only.

**Design (7b, file paths, default off).** `[links] open_files = false`. Pattern `path[:line[:col]]`, resolved against the
pane's `#{pane_current_path}` at click time, existence-checked, opened with `[links] file_command` (empty means `xdg-open`)
with `{path} {line} {col}` substituted per argv element, no shell, and a `./` prefix when a path starts with `-`.

**Knobs.** `[links] enabled = true`, `open_files = false`, `file_command = ""`; Settings rows under General.

**Tests.** URL regex table (trailing punctuation, parentheses, markdown links); scheme allowlist; substitution and injection
cases (`;`, spaces, leading `-`, `$()`); the wrapped-URL join as a pure function. Headless: a debug-only
`HASHTERM_TEST_OPEN_LOG` records the URI instead of launching (debug builds only, like `HASHTERM_SOCKET`); Ctrl+click on a
printed URL logs exactly that URI, a plain click logs nothing, `printf` OSC 8 through tmux is clickable.

**Risks.** Regex false positives (mitigated by the tooltip); a running server picks up the conf line only through the
startup apply; link text can differ from its target (mitigated: the tooltip shows the target).

### Item 9: notifications and attention badges (0.4.0, 2.5 d [E])

**Outcome.** With the drop-down hidden, a bell becomes a desktop notification; groups show when another tab needs you.

**Facts [V].** With a client attached to a one-window session (the shape of a hashterm tab [I]) `alert-bell` fires but
`alert-activity` and `alert-silence` do not, so `Badge::Activity` (`tabbar.rs:22-26`, set at `app.rs:533`) is
effectively dead code. VTE emits `contents-changed` for a tab hidden in the stack and after the whole window is hidden.
`set_badge` ignores the active tab even when the window is hidden (`window.rs:2406-2410`). `GroupInfo` carries only
name, colour and count (`tabbar.rs:30-34`). The app registers a GApplication id (`app.rs:40-43`) and ships a `.desktop`
file with `StartupNotify` (`assets/com.redasgard.Hashterm.desktop`).

**Design.**
- **Seen** = the tab is active, the window is visible, and the window is active. Anything else is unseen.
- **Attention model:** per tab `Option<Attention { kind: Bell | Activity | External }>`. Bell comes from the existing hook.
  Activity comes from VTE `contents-changed` on an unseen tab, with leading-edge arming and a 5 s quiet period before it can
  re-arm, so `top` does not stay lit. Cleared by `select_tab` (`window.rs:1383`) or when the window becomes visible and
  active on that tab. Keep `TmuxEvent::Activity` in the wire format (stability) but stop relying on it.
- **Group badge:** `GroupInfo.attention` is the highest-priority kind among the group's unseen tabs (Bell > Activity >
  External). The group row gets a dot (`.attn-bell`, `.attn-activity`) with colours from F1 and a tooltip ("2 tabs need
  attention"). The existing `reveal_on_badge` (`window.rs:2640`, `2673`) extends to the new signal.
- **Desktop notifications** via `gio::Notification` and `Application::send_notification` (no new crate). Knobs:
  `[notifications] enabled = true`, `on_bell = true`, `on_activity = false`, `only_when_unseen = true`,
  `min_interval_secs = 10`. Body: "group › tab: bell". The default action calls an app action `show-tab` with the
  session id, which presents the window and selects the tab. The notification is withdrawn when the tab is seen.
  Rate-limited per tab.
- Per-tab mute is deliberately out of scope for now (add it if bells prove noisy).

**Not verified [I].** That GNOME actually displays notifications for this app id, and the click-to-activate round trip.
Test on the real desktop.

**Tests.** Pure functions: group aggregation, the quiet-period arming with an injected clock, the rate limiter. Headless:
`printf '\a'` in a background tab sets the tab and group attention (assert via a debug log line); notification contents
captured on the private session bus with `dbus-monitor "interface='org.freedesktop.Notifications'"`; the click action driven
with `gdbus call ... org.gtk.Application ActivateAction`.

**Acceptance.** Hidden window plus a bell in the active tab gives one notification within 1 s; clicking it shows the window
on that tab. Visible window on that tab gives none. A bell in another group lights that group's row until visited.
Background output lights the activity badge only after 5 s of quiet. No activity notifications by default.

**Risks.** Notification spam (rate limit and defaults); `contents-changed` frequency (VTE coalesces, the handler is O(1)).

### Item 10: splits as tmux panes (0.4.0, 2.5 d [E])

**Outcome.** Split the current tab into panes with keys; tmux draws and manages them; they survive crashes and restores.

**Facts [V].** `split-window -h|-v -c '#{pane_current_path}'`, `select-pane -L|-R|-U|-D`, `resize-pane -Z`, `kill-pane`
and the `pane-border-*` options all work on 3.5a (`select-pane -L` was exercised; -R/-U/-D share its code path [I]).
With `-c '#{pane_current_path}'` the new pane starts in the active pane's folder; **without `-c` it starts in the
directory the tmux server was launched from**, so every split call must pass it. Session restore already rebuilds panes
and layouts (`restore.rs:149-172`, `dump.rs:128-161` [R]).

**Design.**
- **Hotkeys** (via F2): `split_right = "<Ctrl><Shift>e"`, `split_down = "<Ctrl><Shift>o"`,
  `pane_left/right/up/down = "<Alt><Shift>Left/Right/Up/Down"`, `zoom_pane = "<Ctrl><Shift>x"`. Alt+arrow alone is avoided
  because readline uses it for word movement.
- **Actions** run the tmux commands above against the active pane id (F6), off-thread, then `queue_redraw`. A refused split
  (tmux: "no space for new pane") shows tmux's message as an OSD. Zoom toggles show an OSD, since there is no status line.
- **Closing (D1).** `Ctrl+Shift+W` closes the active pane when the tab has more than one, else the tab exactly as today
  (the contract in `after_tab_removed` is unchanged). The tab's "x" always closes the whole tab, and asks first when it
  holds more than one pane ("Close tab with N panes?", Cancel default) using `confirm()` (`window.rs:1243`). Pane count
  comes from F6's cached `window_panes`.
- **Borders** use item 8's tmux styles. Titles, cwd and the search target follow the active pane.
- **No new GTK layout.** A tab stays one session and one VTE client; tmux draws the panes. Mouse focus and border dragging
  come from tmux `mouse on` [I: confirm headless].

**Tests.** Builder unit tests; scratch-server integration: split, count, select, zoom, kill, then dump, kill server and
restore with the pane count preserved; headless: Ctrl+Shift+E gives 2 panes (`#{window_panes}`), Alt+Shift+Left changes
`pane_active`, Ctrl+Shift+W closes the pane then the tab with the previous-tab rule intact.

**Acceptance.** All of the above, plus: closing the last pane by shell exit follows the existing contract; a restore keeps
the split layout.

**Risks.** Key conflicts with applications or input methods (all rebindable; Ctrl+Shift+E may clash with an IME emoji
shortcut [I]); the close semantics (D1).

## 7. Settings, config and docs checklist

New config keys, all with defaults, each needing a `config.rs` field, an `assets/default-config.toml` entry, a Settings row
(where user-facing) and a README/docs line:

| Section | Keys |
|---|---|
| `[general]` | `log_to_file` |
| `[appearance]` | `scheme`, `follow_system`, `scheme_dark`, `scheme_light`, `cursor_shape`, `cursor_blink`; `foreground`/`background` become optional overrides |
| `[tabbar]` | `auto_titles` |
| `[notifications]` (new) | `enabled`, `on_bell`, `on_activity`, `only_when_unseen`, `min_interval_secs` |
| `[links]` (new) | `enabled`, `open_files`, `file_command` |
| `[hotkeys]` | `new_group`, `search`, `zoom_in`, `zoom_out`, `zoom_reset`, `split_right`, `split_down`, `pane_left`, `pane_right`, `pane_up`, `pane_down`, `zoom_pane` |
| `UiState` | `hints_seen`, `font_scale` |

Docs to update: README (Features, Default keys, Install: tmux >= 3.5; `user.css` note), `docs/settings.md` (page table),
`docs/RELEASING.md` (CI line), `packaging/build-deb.sh` (`Depends: tmux (>= 3.5)`), man page `packaging/hashterm.1`
(`--doctor`). Screenshots to re-capture after items 1, 4 and 8: hero, matrix, dial, menus, all Settings pages, demo GIF.

## 8. Verification plan

- **CI** (item 5): fmt, clippy `-D warnings`, tests, with tmux present. Every item adds unit and scratch-tmux tests as
  listed.
- **Headless UI harness:** a private `HOME`/`XDG_*`/`TMUX_TMPDIR`, Xvnc, xfwm4 with the compositor off, xdotool, and PIL
  screenshots with pixel-measured contrast. Never touch the live `-L hashterm` server.
- **Matrix for chrome checks:** GTK theme (light, dark) x tab-bar edge (top, left) x groups (1, 3) x scheme (default, one
  light, one dark).
- **Manual, on the real GNOME Wayland desktop** (cannot be automated): the portal consent dialog and hotkey status text;
  that notifications appear and their click action works; dialog placement; hide-on-focus-loss; the group-close hover;
  Settings visuals; XWayland sharpness with the new colours.
- **Not yet verified anywhere:** behaviour on GNOME 50, Fedora 44 and Ubuntu 26.04 (the compat-matrix CI job is a separate,
  deferred item).

## 9. Release plan

- **0.3.0** = Step 0, 5, 3, 1, 2, 4. Cut `v0.3.0-rc1` first (a tag containing `-` becomes a pre-release in the draft flow of
  `docs/RELEASING.md`), run the manual desktop checks, then tag `v0.3.0`.
- **Fix before the first rc.** `packaging/build-deb.sh` uses the `Cargo.toml` version verbatim, so `0.3.0-rc1` becomes the
  Debian version `0.3.0-rc1-1`, which dpkg sorts **after** the final `0.3.0-1` [V]: an installed rc would refuse the upgrade
  to the release. Map the first `-` to `~` for the Debian version only (`0.3.0~rc1-1` sorts before `0.3.0-1` and after
  `0.2.2-1` [V]). The tag, the tag-vs-`Cargo.toml` check and the `hashterm_*.deb` globs (`slsa-release.yml:96,101`) need no
  change. Check that GitHub keeps the `~` in the uploaded asset name [I]; the provenance digest does not depend on it. Add a
  `dpkg --compare-versions` assertion to CI.
- **0.4.0** = 8, 6, 7, 9, 10, with the same rc step.
- Compatibility: every new key defaults to current behaviour; old configs load (`foreground`/`background` stay valid as
  overrides). The only packaging change is `Depends: tmux (>= 3.5)`. Downgrades do not work: `Config` and its sections use
  `#[serde(default, deny_unknown_fields)]` (`config.rs:50`), so a 0.4 `config.toml` containing new keys fails to load in 0.3
  or earlier. The Settings window already keeps a one-time `.bak`; say this in the release notes.

## 10. Risks

| Risk | Where | Mitigation |
|---|---|---|
| GTK CSS differences across minor versions | 1, 8 | Re-run the state matrix on GTK 4.22; floor stays 4.18 |
| Notifications not shown on GNOME | 9 | Real-desktop test before 0.4.0; the group badge works regardless |
| Search latency on huge histories | 6 | Off-thread, debounce, generation counter; measured 50-131 ms at 50k lines |
| Link false positives or spoofed OSC 8 text | 7 | Ctrl+click only, scheme allowlist, tooltip shows the target |
| Key conflicts | 6, 8, 10 | All new keys configurable; F2 conflict detection; documented |
| `foreground/background` becoming optional | 8 | Old files stay valid; defaults test; Settings rows tested |
| Title polling cost | 2 | One call per 2 s, visible only; measure at 50 tabs |
| Scheme licences | 8 | Verify upstream licences at bundling; record provenance |
| `window.rs` (3,118 lines, 25 `borrow_mut` calls) | 2, 4, 9, 10 | Add new behaviour in new modules; keep borrows short; the `RefCell already borrowed` pitfall applies |

## 11. Decisions for the owner (defaults chosen; veto any)

- **D1.** Ctrl+Shift+W closes a pane before it closes a tab (single-pane tabs behave exactly as today). The tab's "x"
  closes the whole tab and confirms when it has several panes.
- **D2.** The group bar stays hidden for a single group; discoverability comes from the "+ Group" chip and the first-run
  hint.
- **D3.** Chrome follows the colour scheme, not the GTK theme; dialogs and Settings keep following the system theme.
- **D4.** Notifications default to bell only; activity notifications are off.
- **D5.** Clickable file paths ship off by default.
- **D6.** Default shortcuts as listed per item (Ctrl+Shift+F, Ctrl+Shift+N, Ctrl+Shift+E/O/X, Alt+Shift+arrows,
  Ctrl+plus/minus/0).
- **D7.** tmux floor raised to 3.5 (the `.deb` `Depends` and README) instead of gating features by version. No currently
  supported distro is excluded: Debian 13 ships 3.5a, Ubuntu 25.04 and 25.10 3.5a, 26.04 3.6a; Ubuntu 24.04 has 3.4, but its
  VTE 0.76 is already below the `.deb`'s `>= 0.80` floor [V].

## 12. Deferred, with triggers

- **Jump-to-prompt and command-finished alerts** through tmux OSC 133. Prompt navigation exists since tmux 3.4; the
  `pane-command-finished` hook is 3.8-only (released 2026-09-09 [V]); Debian 13 ships 3.5a, Ubuntu 26.04 3.6a. Revisit
  when 3.8 is common or as an opt-in with feature detection.
- **Distribution** (compat-matrix CI job, AUR, COPR, signed APT repo; not Flathub). Revisit after 0.3.0 is out.
- **Native-Wayland GNOME Shell extension.** Revisit if GTK removes its X11 backend in a 4.x release or the XWayland path
  breaks on a supported distro.
- **tmux -CC, sixel and kitty graphics, per-tab notification mute.**

## Appendix A: spike results (2026-10-15; tmux 3.5a, GTK 4.18.6, VTE 0.80; all numbers re-measured for the final version)

| Spike | Result |
|---|---|
| Default `pane_title` | equals the host name; `pane_current_path` and `pane_current_command` follow a `cd` and a running `sleep` (0.5 s settle) |
| Title follows `cd`, `status off` | first `cd` after 3.81 s (3.52, 3.51, 3.51 s in three runs at `status-interval` 15, 1, 2); the three `cd`s after it never arrived within 20 s each; a foreground program name never arrived |
| Title follows `cd`, `status on`, interval 1 | 0.8 s |
| Program-set title (OSC 2) | 0.05 s; survives a later `cd` |
| Search, 49,973-line history | one match deep in the history 131 ms; no match 50 ms; 1,111 matches ("line 49") 91 ms; 50,001 matches ("filler") 65 ms; count read 4-5 ms |
| `search_count` | exact (1,111 and 50,001); `search_count_partial` 0; the binary has four search formats (`search_count`, `search_count_partial`, `search_match`, `search_present`) and no current-match index; stale after a miss in the same copy-mode session |
| Search case rules | smart-case: "hello world" matches "Hello World"; "HELLO WORLD" and "LINE 4242 " match nothing |
| copy-mode style options | `copy-mode-match-style`, `copy-mode-current-match-style`, `mode-style`, `copy-mode-mark-style` accepted |
| OSC 8 through tmux | dropped by default; forwarded with `terminal-features ',xterm-256color:hyperlinks'` |
| tmux alert hooks, attached client, one window | `alert-bell` fires (with `monitor-activity` on or off); `alert-activity` never; `alert-silence` none in 5 s |
| Splits | `split-window -h/-v`, `select-pane -L`, `resize-pane -Z`, `kill-pane`, `pane-border-style`, `pane-active-border-style`, `pane-border-lines`, `pane-border-indicators` all accepted; with `-c '#{pane_current_path}'` the new pane starts in `/tmp`, without `-c` in the server's launch directory |
| One `display-message` call | median 3.8 ms, p95 4.6 ms, max 7.1 ms |
| One `list-panes -a -F` (8 fields) | 4 panes: median 3.5 ms (p95 5.2); 51 panes: 5.2 ms (p95 6.3); 101 panes: 5.7 ms (p95 7.6, max 8.7) |
| Ctrl+click under mouse tracking | reaches the child (1 report); a capture-phase `GestureClick` that claims Ctrl presses blocks it (0); plain clicks unaffected (2 of 2). An `EventControllerLegacy` variant raised an exception in the harness and is unvalidated |
| VTE `contents-changed` | fires for a stack-hidden terminal and after the whole window is hidden (events 0.8 s and 2.8 s after hiding) |
| Chrome CSS, current stylesheet | table in item 1 (pixel-measured under the default GTK theme and `Adwaita:dark`) |
| Chrome CSS, candidate (default-scheme tokens) | every element and state (rest, hover, checked, checked+hover, backdrop) renders the token colours, identical under both themes; labels 5.88-9.67:1, icons 4.57-10.53:1; weakest: the close icon on the active tab, 4.57:1 |
| Token derivation, 14 palettes | worst cases: muted label 5.23 on the strip, 4.87 on hover and 4.52 on the active fill; active label 7.04; active fill vs strip 1.30 (dark) and 1.16 (light); group label 5.04 |
| `cargo test --workspace` | 67 pass in 3 s, no display; the first attempt failed at `resurrect.rs:49` with "File name too long" until `TMUX_TMPDIR` was short |
| clippy 1.96.0 | 0 errors, 5 warnings (63 s) |
| rustfmt 1.9.0 (default toolchain, edition 2024) | 13 of 36 tracked files differ, 73 hunks; `cargo fmt` is not installed under the pinned toolchain |
| Distros | tmux: Debian 13 3.5a, Ubuntu 25.04 3.5a, 25.10 3.5a, 26.04 3.6a (24.04: 3.4). VTE: Ubuntu 24.04 0.76 (below the `>= 0.80` floor), 25.04 0.80.0, 25.10 0.80.3, 26.04 0.84.0 |
| Offline registry | absent: `tracing-appender`, `shell-words`, `palette`; present: `regex`, `url`, `notify-rust`, `zbus`, `toml_edit`, `tracing-subscriber`, `serde_json`; `ashpd` 0.13.13 has a dependency-free `settings` feature |
| Release candidates in dpkg | `0.3.0-rc1-1` sorts after `0.3.0-1`; `0.3.0~rc1-1` sorts before it and after `0.2.2-1` |

## Appendix B: reference rule set for item 1 (proven in the spike; values shown are the default scheme's tokens)

```css
.console-body { background-color: #1d1f21; }
.tabbar   { background-color: #17181a; padding: 2px; }
.groupbar { background-color: #111213; padding: 2px; }
.tab, .group-row {
  padding: 3px 10px; border-radius: 6px;
  background-image: none; background-color: transparent; border-color: transparent;
  box-shadow: none; text-shadow: none; color: #949796;           /* muted */
}
.tab:hover, .group-row:hover { background-color: #222425; color: #c5c8c6; }
.tab:checked, .group-row:checked,
.tab:checked:hover, .group-row:checked:hover { background-color: #2d2f30; color: #d9dbda; }
.group-row { color: #939594; }                                    /* group_muted */
.tab-new, .tab-close, .group-new, .group-close {
  background-image: none; background-color: transparent; border-color: transparent;
  box-shadow: none; color: #949796;
}
.group-new, .group-close { color: #939594; }
.tab-new:hover, .tab-close:hover, .group-new:hover, .group-close:hover { background-color: #222425; color: #c5c8c6; }
```

The per-tab accent underline (`.tab.acc-<hex> { box-shadow: inset 0 -3px 0 0 ... }`, `window.rs:2860`) sits at
APPLICATION+2 and outranks these rules by design.
