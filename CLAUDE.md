# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`reviewal` is an adversarial multi-agent review TUI (Rust, ratatui + tokio). It fans out multiple `claude` CLI subprocesses — one per reviewer persona — over a spec or git diff, streams their activity live, lets the user triage findings, and writes a report that the companion `reviewal-ingest` skill pulls back into the authoring Claude session. Review methodology: orthogonal personas, an optional cross-review round, and deterministic synthesis.

## Commands

```bash
cargo test                                        # full suite; hermetic — no network, no claude CLI needed
cargo test <name>                                 # single test by name substring
cargo test --test cli                             # just the CLI integration tests
REVIEWAL_LIVE=1 cargo test --test live -- --nocapture  # one real end-to-end run (~2 model calls, needs claude on PATH)
cargo install --path .                            # install the binary
```

The live test is guarded by the `REVIEWAL_LIVE` env var (not `#[ignore]`), so plain `cargo test` always skips it.

When you finish a piece of work the user is going to check by hand, run `cargo install --path .` as the final step so their installed `reviewal` binary matches the working tree — done ≠ delivered until it's installed.

## Architecture

Two halves, bridged by channels:

- **`src/engine/`** — async (tokio). `run::execute_run` is the orchestrator: collect target → round 1 (all personas in parallel via `JoinSet`) → optional round-2 cross-review → deterministic synthesis. It emits `RunEvent`s over an unbounded mpsc channel and honors cancellation via a `watch` channel (`join_or_cancel`). Two engine invariants worth protecting: each persona persists its own round output to disk the moment it finishes (incremental round-1 persistence — a cancel or crash never loses a completed review), and a user cancel ends with `RunEvent::RunCancelled { kept_reviews, resumable }`, never a failure event — a cancelled run with ≥2 saved reviews lands in `ReviewsComplete` and resumes into triage.
- **`src/ui/`** — synchronous ratatui loop (`ui/mod.rs::run_tui`): draw, drain engine events, poll keys at 50ms. `app.rs` holds the screen state machine — `Screen::{Home, Composer, Dashboard, Triage, Done}` — with all navigation expressed as a `Transition` enum applied in `App::apply`. `EngineHandle` (in `app.rs`) owns the tokio runtime and is the only bridge between the sync UI and async engine. Key grammar is uniform across screens: `q` quits wherever bound, `esc` goes back (clearing any filter/overlay first), `?` opens help on Home and Triage; cancelling a live run is two-step (`c` arms, `c` confirms, anything else disarms), and `Ctrl+C` routes into that same confirmation instead of quitting.

Key engine modules:

- **`agent.rs`** — spawns one `claude -p --safe-mode --tools "" --no-session-persistence --json-schema …` subprocess per invocation and parses its `stream-json` stdout into `AgentActivity` events. The hardening flags are a security invariant: text inside the reviewed artifact must not be able to trigger tools, hooks, or MCP. stderr is drained in a separate task to avoid pipe-buffer deadlock (there's a regression test for this).
- **`run.rs::run_persona`** — invoke → extract JSON → validate, retrying exactly once with the validation error appended to the prompt. A run aborts if fewer than 2 personas produce valid round-1 output; round-2 failures are soft (the persona's round-1 review still counts). Failed personas land in `RunRecord.degraded`.
- **`synthesis.rs`** — deliberately deterministic (no LLM call): dedupes findings by normalized title + file/line, then scores confidence (`CrossValidated` / `Consensus` / `Disputed` / `Solo`) from who reported vs. validated vs. challenged. Verdicts average into SHIP / SHIP-WITH-CAVEATS / HOLD / BLOCK.
- **`store.rs`** — everything persists under `.reviewal/runs/<id>/` (`run.json`, `source.txt`, `round1/`, `round2/`, `triage.json`, `report.{md,json}`); `runs/latest` is a plain-text pointer file. Run lifecycle: `Running → ReviewsComplete → Finalized`, or `Aborted`; on TUI startup `mark_stale` flips any leftover `Running` runs to `Stale`. Only `ReviewsComplete` runs resume into triage; triage saves eagerly on every keypress that dirties it.
- **`persona.rs`** — 6 built-ins. Custom personas are markdown with TOML frontmatter loaded from `~/.config/reviewal/personas/` (global) then `.reviewal/personas/` (project); later dirs win on name collision, and a custom persona can shadow a builtin. Custom personas carry a `source` path; load failures are structured (`PersonaLoadError`, not a bare string) and render as invalid checklist rows; `builtin_source` exposes byte-exact embedded text for materialization.
- **`preflight.rs`** — checks the installed `claude` CLI's `--help` for required flags before a run. The TUI runs `spawn_check_claude` asynchronously at startup (drained by `App::poll_preflight`, so drawing never blocks on a subprocess); `reviewal review` runs the full `preflight` in `main` *before* the terminal guard, so scripts get preflight errors on stderr with exit 2 even when stdout is a pipe.

Key ui modules:

- **`ui/home.rs`** — three tabs in one content box (`tab`/shift-tab cycle, `1/2/3` jump): *start a review* (detected diff/spec targets, Enter = quick-start, a dim `personas: …` detail line under the selected row), *personas* (the full library via a filter-less `PersonaManager` — same v/e/n/d/x verbs as the composer checklist, no per-run toggle), and *history* (runs; `◐ N` triage-debt badge on the tab label). Each tab's box hugs its content, capped at the footer.
- **`ui/personas.rs`** — the shared persona-management component (`PersonaManager`): rows + cursor, pager, `[p]/[g]` scope prompt, armed-delete grammar, staged `$EDITOR` requests, and `on_editor_return`. Embedded by both the composer checklist (filtered to the run's target kind) and the home personas tab (unfiltered, `persona::available_all`); `run_tui` drains editor requests through `App::take_pending_editor`/`editor_returned`, dispatching to whichever screen owns the manager.
- **`ui/composer.rs`** — single-screen run setup: each decision (target, reviewers, model, cross-review) is a value + dim-description block, with a `start review` action row last. Enter/space *activates* the focused row — opens its inline editor (target list, reviewer checklist, model list), toggles cross-review — and only the start-review row launches the run. Layout degrades airy → compact → minimal on short terminals (hints/error rows always survive); all line budgets are display-column-based (`format::truncate_*`). The spec-picker renders as a full-screen modal, not an overlay. Owns `collect_spec_files`. The reviewer checklist manages personas in place — `v` pager, `e` edit (builtins materialize via a `[p]/[g]` scope overlay, write-if-absent), `n` new from template, `d` duplicate, `x` two-step delete (`esc` consumed while armed; deleting a project file that shadows a builtin resets to the builtin, or resurfaces a global copy of the same name if one exists); edits run through `$EDITOR` via a staged `EditorRequest` drained by `run_tui` (`ui/editor.rs`, RAII suspend), with post-edit identity keyed by file path in `on_editor_return`; the checklist's state and verbs are the shared `PersonaManager` (`ui/personas.rs`) plus a composer-owned space-toggle.
- **`ui/editor.rs`** — `$EDITOR` shell-out for persona edits: resolves `$VISUAL` → `$EDITOR` → `vi`, runs it via `sh -c` (blocking) so multi-word commands like `code --wait` work, and `SuspendGuard` is the RAII inverse of the TUI's terminal guard — cooked mode + primary screen on construction, alternate screen + raw mode restored on every exit path, spawn failures included.
- **`ui/format.rs` / `ui/overlay.rs`** — shared render helpers: relative times, progress bars, word-wrap row counts (char-based), column-based `truncate_path_start`/`truncate_end`; centered overlays and the `?` help box.
- **`ui/theme.rs`** — semantic color roles (`Theme`), built once via `Theme::load(&config)` → `(Theme, warnings)`; persona colors resolve on demand with `theme.persona_color(name, frontmatter_color)` (builtin slots, then FNV-1a hash into the accent-filtered pool). Each role carries an ANSI default and a curated 24-bit default — `COLORTERM` terminals get the RGB palette, `[theme] truecolor` forces it either way, `NO_COLOR` ⇒ monochrome. `[theme]` config keys merge per-field, never whole-table. Shared chrome lives here too: `theme::bordered()`/`Theme::panel`/`inset_title` (rounded boxes, `╭─ title` inset) and `Theme::selection_style()` (bg-tint row selection, REVERSED in mono) — screens never hand-roll borders or selection.

Other pieces: `config.rs` layers project `.reviewal/config.toml` over global `~/.config/reviewal/config.toml` (XDG-aware), and `config::load` is the *only* place ambient environment is read — it records the global persona dir plus `env_no_color`/`env_truecolor` (NO_COLOR, COLORTERM) on `Config`, and everything downstream (UI, engine, CLI) resolves persona directories through `Config::persona_dirs` and colors through `Theme::load(&config)`, never env. This is a hermeticity invariant: a `Config::default()` has no global dir and plain ANSI color, so tests can't be broken by whatever lives in the developer's real `~/.config/reviewal/personas/` or shell exports. `skill.rs` implements `reviewal init`, which installs `assets/SKILL.md` (embedded via `include_str!`, version-stamped with `reviewal-version:` for upgrade detection) into `.claude/skills/reviewal-ingest/`.

## Testing conventions

- Tests live inline in each module (`#[cfg(test)]`), plus `tests/cli.rs` (binary-level, via `CARGO_BIN_EXE_reviewal`) and `tests/live.rs`.
- Engine tests never call the real CLI: they write small executable shell scripts to a tempdir and pass the script path as `claude_bin` (see `HAPPY_SCRIPT` in `run.rs` tests, and similar in `agent.rs`/`preflight.rs`). Follow this pattern for new subprocess behavior.
- UI tests render frames to plain text with `app::render_to_text` and assert on the strings — no terminal needed.
- `tests/fixtures/agent-output/` holds the contract fixtures for `parse.rs::extract_json` (banner-prefixed, fenced, wrapper-object variants of agent output). Add a fixture when handling a new output shape.
