# reviewal

An adversarial-review TUI. Point it at a spec, a plan, or a git diff; it fans out
multiple Claude reviewer agents — each with a deliberately different attack angle —
watches them work live, lets you triage what they find, and hands the synthesized
report back to the Claude session that authored the artifact.

The review methodology: orthogonal personas, an optional cross-examination round,
and a deterministic consensus-scoring synthesis instead of a fourth LLM call.

## The workflow it closes

```
you author a spec/plan/diff in a Claude session
        │
        ▼
   reviewal            ← launcher: Enter runs a detected target with defaults;
        │                 e/n opens the composer (target, reviewers, model,
        ▼                 cross-review — all on one screen)
   live dashboard      ← one column per reviewer, streaming activity
        │
        ▼
   triage              ← accept / dismiss-with-note / defer each finding
        │
        ▼
   finalize            → .reviewal/runs/<id>/report.{md,json} + runs/latest
        │
        ▼
   /reviewal-ingest    ← back in your authoring session: pulls accepted findings
```

Dismissed findings stay in the report with your notes — the authoring session sees
what was ruled out and why, and won't re-litigate it.

## Install

Prebuilt binaries (macOS and Linux, no Rust required):

```bash
curl -LsSf https://github.com/oksr/reviewal/releases/latest/download/reviewal-installer.sh | sh
```

With a Rust toolchain, from [crates.io](https://crates.io/crates/reviewal):

```bash
cargo install reviewal       # or `cargo binstall reviewal` to skip the compile
```

From source:

```bash
git clone https://github.com/oksr/reviewal
cd reviewal
cargo install --path .
```

Requires the [`claude` CLI](https://claude.com/claude-code) on PATH (a version
with `--json-schema`, `--safe-mode`, and `--tools`; preflight checks and tells
you if yours is too old), and `git` for diff reviews.

## Quickstart

```bash
cd your-project
reviewal init      # installs the reviewal-ingest skill + scaffolds .reviewal/
reviewal           # open the launcher; Enter starts the highlighted target
```

Home detects reviewable targets (uncommitted diff, branch vs main/master,
spec files) and lists past runs. `Enter` quick-starts the highlighted target
with default reviewers; `e` opens the composer pre-filled with it; `n`
composes from scratch; `Tab` switches to run history (resume an interrupted
triage, or open a finalized run's report).

Or skip the launcher entirely:

```bash
reviewal review --diff              # review uncommitted changes
reviewal review --diff main         # review branch vs main
reviewal review --spec docs/plan.md # review a spec/plan
reviewal review --diff --cross-review --personas prover,breaker
```

## Commands

| Command | What it does |
|---|---|
| `reviewal` | Open the TUI at Home (launcher + run history; `e`/`n` open the composer) |
| `reviewal review [--diff [BASE] \| --spec PATH...] [--personas a,b] [--cross-review] [--model M]` | Jump straight into a run |
| `reviewal init [--force]` | Install the companion skill project-locally, scaffold `.reviewal/`, gitignore `runs/` |
| `reviewal personas` | List built-in and custom personas |

`reviewal` and `reviewal review` need an interactive terminal — with stdout
redirected they refuse to start rather than fail inside terminal setup.
`reviewal review` runs preflight *before* that check, so a script still gets
target and CLI errors on stderr. Exit codes: **2** for bad arguments,
preflight failures, or a non-terminal stdout; **1** for init or terminal
failures.

## Personas

| Persona | Target | Lens |
|---|---|---|
| prover | code | Correctness: logic, edge cases, and invariants |
| breaker | code | Security: attack surface, abuse, and trust boundaries |
| steward | code | Maintainability: complexity, coupling, and future cost |
| skeptic | spec | Feasibility and hidden complexity |
| stickler | spec | Ambiguity, contradictions, and missing requirements |
| advocate | spec | User outcome and product fit |

Custom personas are markdown files with TOML frontmatter, dropped in
`.reviewal/personas/` (project) or `~/.config/reviewal/personas/` (global);
project wins on name collision, and a custom persona named like a built-in
replaces it:

```markdown
+++
name = "perf-hawk"
title = "Perf Hawk"
lens = "Latency and allocation regressions"
target = "code"   # code | spec | both
+++
You are the Perf Hawk, one of the reviewers in an adversarial review...
```

Keep custom personas orthogonal to the ones they run beside — overlap costs
model calls for no signal.

You don't have to touch the filesystem yourself: in the composer's reviewer
checklist, `v` opens a read-only view of any persona's source, `e` edits it
in `$EDITOR` (editing a built-in first asks whether to materialize the copy
into the project or global personas directory), `n` starts a new persona from
a template, `d` duplicates one, and `x` deletes it (press again to confirm;
a pristine built-in has no file to delete, so it gets a "built-in — e edits
a copy" notice instead). Each row is tagged with where it came from —
built-in, project, global, edited, or invalid if the file failed to parse.

## How a run works

1. **Round 1** — every persona reviews the artifact independently, in parallel.
2. **Round 2 (optional, off by default)** — each persona sees all round-1 reviews
   and must validate or challenge the others' findings. Toggled per run in the
   composer (or `--cross-review`).
3. **Synthesis (deterministic, no LLM)** — findings are deduped and scored:
   *cross-validated* (reported by ≥2), *consensus* (validated by another),
   *disputed* (challenged), *solo*. Verdicts average into SHIP / SHIP-WITH-CAVEATS
   / HOLD / BLOCK.
4. **Triage** — you accept/dismiss/defer; only then is the report finalized.

Reviewer subprocesses are hardened: `claude -p --safe-mode --tools "" 
--no-session-persistence --json-schema …` — no hooks, no MCP, no CLAUDE.md, no
tool use, schema-enforced output. Text inside the reviewed artifact cannot
trigger side effects.

## In the TUI

The key grammar is uniform: `q` quits wherever it's bound (over a live run
you cancel with `c` first — `q` won't kill reviewers mid-flight), `esc` steps
back (clearing the active filter or overlay first, if there is one), and `?`
opens a key-reference overlay on Home and Triage. `Ctrl+C` quits — except
over a live run, where it routes into the same cancel confirmation as `c`.

- **Cancelling is two-step**: `c` arms a confirmation showing what would be
  kept and what would be lost; a second `c` confirms, any other key disarms.
  Every review that finished before the cancel is already on disk — if at
  least two finished, the cancelled run is resumable straight into triage
  (and stays resumable from Home later).
- **Degraded runs pause to tell you**: if some reviewers failed but at least
  two succeeded, the dashboard stops on a summary naming who failed and what
  you're triaging without, instead of silently advancing.
- **Triage is an inbox**: findings you've touched are marked, `u` undoes,
  `/` filters (the filter is sticky and shown while active), and `f` asks for
  confirmation — with accepted/dismissed/untriaged counts — before finalizing.
  Finalizing isn't final: open the run from Home and press `r` on its
  summary to reopen triage.

Startup warnings — invalid theme values, corrupt run records, or failures
marking an interrupted run stale — surface directly on the Home screen rather
than vanishing into stderr.

## Configuration

`.reviewal/config.toml` (project) overrides `~/.config/reviewal/config.toml`
(global):

```toml
model = "opus"        # default: the claude CLI's own default model
timeout_secs = 600    # per agent invocation
claude_bin = "claude" # override to pin a specific binary (global config only)
```

`claude_bin` is honored only from the global config: the project file ships
with the checkout being reviewed, and a hostile repo must not choose which
binary gets executed. A project-level `claude_bin` is ignored with a warning.

Everything a run produces lives under `.reviewal/runs/<id>/` (`run.json`,
`source.txt`, per-persona round output, `triage.json`, `report.md`,
`report.json`); `runs/latest` points at the newest finalized run. Runs that
crashed after reviews finished show up on Home as resumable.

## Theming

On terminals that advertise 24-bit color (`COLORTERM=truecolor`), reviewal
uses a curated dark RGB palette; everywhere else it falls back to ANSI-16 and
inherits your terminal theme. `truecolor = true` / `false` under `[theme]`
forces the choice either way (useful under tmux configs that hide
`COLORTERM`, or on light terminals where the dark palette fits poorly), and
`NO_COLOR` still wins with monochrome. Every role is overridable in `[theme]`
— project `.reviewal/config.toml` merges over global
`~/.config/reviewal/config.toml` per key. The ANSI defaults:

```toml
[theme]
# accent = "blue"                    # app chrome: title, borders, selection pointer, hint keys
# dim = "gray"                       # secondary text
# selection_bg = "dark gray"         # selected-row background tint
# error = "red"
# status_pending = "gray"            # dashboard agent states (running wears the persona color)
# status_retrying = "yellow"
# status_done = "green"
# status_failed = "red"
# run_status_running = "cyan"        # home-screen run list
# run_status_reviews_complete = "yellow"
# run_status_finalized = "green"
# run_status_aborted = "red"
# run_status_stale = "gray"
# severity_critical = "red"
# severity_warning = "yellow"
# severity_info = "blue"
# confidence_cross_validated = "green"
# confidence_consensus = "cyan"
# confidence_disputed = "yellow"
# confidence_solo = "gray"
# verdict_ship = "green"
# verdict_caveats = "yellow"
# verdict_hold = "light red"
# verdict_block = "red"
# persona_pool = ["cyan", "magenta", "blue", "yellow", "green", "light blue", "light magenta"]
```

Values accept ANSI names (`"cyan"`, `"light red"`), hex (`"#ff5555"`), or
indexed (`"13"`). Invalid values keep the default and show a warning on the
home screen.

Each reviewer persona gets a signature color from `persona_pool` (minus the
accent — changing the accent can therefore reshuffle default persona colors).
Pin a persona's color in its frontmatter:

```toml
+++
name = "redteam"
title = "Red Team"
lens = "attack surface"
target = "both"
color = "light red"
+++
```

## Development

```bash
cargo test                              # full suite; no network, no claude needed
REVIEWAL_LIVE=1 cargo test --test live  # one real end-to-end run (~2 model calls)
```

## License

MIT — see [LICENSE](LICENSE).
