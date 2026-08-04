# Kritikon

[![CI](https://github.com/storozhenko98/kritikon/actions/workflows/ci.yml/badge.svg)](https://github.com/storozhenko98/kritikon/actions/workflows/ci.yml)
[![Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-56c7bc.svg)](LICENSE)
[![GitHub release](https://img.shields.io/github/v/release/storozhenko98/kritikon)](https://github.com/storozhenko98/kritikon/releases/latest)

**Attention for the review queue.**

Kritikon is a focused GitHub pull-request command center for your terminal. Its name comes from the Ancient Greek *kritikon*: the faculty by which judgments are made.

It keeps the three queues that matter in one calm interface:

1. **To Review** — open PRs requesting you directly or any visible team you belong to.
2. **Involved** — open PRs you committed to, reviewed, commented on, were assigned to, or were mentioned in.
3. **My PRs** — every open PR you authored, including drafts and PRs with no reviews.

The list stays compact while the selected PR shows its full review breakdown, outstanding reviewers, draft/ready state, mergeability, CI rollup, branches, files, line changes, comments, labels, and timestamps. Closed PRs are never queried.

## Install

Prerequisites:

- [GitHub CLI](https://cli.github.com/) installed and authenticated with `gh auth login`.
- Apple Silicon macOS, x86_64 Linux, or ARM64 Linux. Windows and Intel macOS are not supported.

Install the latest release without `sudo`:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://storozhenko98.github.io/kritikon/install.sh | bash
```

The installer selects the native binary, verifies its SHA-256 checksum, and places `kritikon` in `~/.local/bin`. To override the destination with `KRITIKON_INSTALL_DIR` or install a specific version with `KRITIKON_VERSION`, download and run the script explicitly:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://storozhenko98.github.io/kritikon/install.sh -o install-kritikon.sh
KRITIKON_INSTALL_DIR="$HOME/bin" KRITIKON_VERSION=0.3.3 sh install-kritikon.sh
rm install-kritikon.sh
```

Then run:

```bash
kritikon
```

You can also build from source with Rust 1.88 or newer:

```bash
cargo install --git https://github.com/storozhenko98/kritikon --locked
```

Kritikon reuses the active `gh` account and host. It never reads or stores your token itself.

## Updates

Interactive release builds check GitHub's latest published release when they start. When a newer semantic version exists, Kritikon offers two explicit choices before opening the dashboard:

- Press `y` to download the native release, verify its SHA-256 checksum, replace the current binary, and exit. Start `kritikon` again to run the new version.
- Press `Enter` to dismiss the prompt and continue with the installed version. Kritikon will offer the update again on a later launch.

An unavailable or slow network never blocks the dashboard for more than three seconds, and update-check failures are silent. Development scenarios, `--check`, and `--reset-config` never prompt.

## Controls

| Action | Keyboard | Mouse |
| --- | --- | --- |
| Move | `↑` / `↓`, `j` / `k`, `PgUp` / `PgDn`, `Home` / `End` | Wheel/trackpad |
| Switch view | `Tab`, `Shift+Tab`, `←`, `→` | Click a tab |
| Open selected PR | `Enter` or `o` | Single-click a PR row |
| Copy selected PR's head branch | `c` | — |
| Copy selected PR's URL | `Shift+C` | — |
| Start/inspect resumable background OpenCode review | `Shift+R` | — |
| Expand/collapse details | `d` or `Esc`; arrows/PgUp/PgDn scroll | Wheel/trackpad |
| Configure refresh timer | `t` | Click editor controls |
| Refresh now | `r` | — |
| Full state legend | `?` | — |
| Quit | `q` or `Ctrl+C` | — |

## Optional OpenCode review agent

`Shift+R` turns the selected PR into a resumable agent-review workspace. This feature is optional: the normal dashboard has no OpenCode dependency.

Requirements:

- `opencode` must be installed, authenticated, and available on `PATH`.
- `gh` must be authenticated to the host containing the selected PR.
- macOS and Linux are supported. Windows is not supported.

Kritikon normally uses your OpenCode default model. Set `KRITIKON_OPENCODE_MODEL` when you want a dedicated reviewer model, for example:

```bash
KRITIKON_OPENCODE_MODEL=opencode/gpt-5.4 kritikon
```

On a PR with no saved agent session:

1. Press `Shift+R`.
2. Leave the focus field blank to use Kritikon's thorough review template, or type additional instructions such as `focus on cancellation and data-loss paths`.
3. Press `Enter`. Kritikon immediately queues the work in the background and remains responsive while it prepares the managed scratch checkout and runs OpenCode headlessly.
4. Press `Esc` to use the rest of the dashboard while the review runs. The footer keeps the background-job count visible and marks completed drafts as ready.
5. Press `Shift+R` on that PR to inspect progress. Once its session is ready, press `o` to attach the real OpenCode TUI if you want to watch or intervene. In OpenCode, press `Ctrl+X`, then `Q` (or run `/exit`) to detach the client and return to Kritikon without stopping the background worker.
6. OpenCode writes the proposed review body to `.kritikon/review.md` without posting anything. Kritikon smoothly replaces the progress view with the rendered Markdown draft when the worker completes.

Every review panel and background job is keyed to the exact PR URL. Dashboard rows show a compact `AGENT PREP`, `AGENT RUNNING`, `AGENT READY`, `AGENT DRAFT`, or `AGENT FAILED` badge for that PR, and unfinished prompts, failures, sessions, and drafts remain independent when you move between PRs.

The draft view supports:

- `r` — run the standard review prompt again in the same OpenCode session.
- `e` — add custom focus and continue the same session.
- `o` — attach the full OpenCode TUI to a running review, or reopen the saved chat after completion, without automatically sending another review prompt.
- `p` — choose **Approve**, **Comment**, or **Request changes**, then pass a separate confirmation screen before Kritikon invokes `gh pr review`.

OpenCode context is preserved by session ID per PR. Reopening the same PR—even after a later review request—continues the prior conversation. Before each run, Kritikon refreshes its disposable checkout to the latest PR head and restores the last saved draft. Active reviews use a password-protected OpenCode server bound only to `127.0.0.1`; the headless worker and optional TUI are separate clients of that server, which is why detaching the TUI does not interrupt the review.

Session records and drafts use native data directories:

- macOS: `~/Library/Application Support/kritikon/review-sessions/`
- Linux: `${XDG_DATA_HOME:-~/.local/share}/kritikon/review-sessions/`

Repository checkouts live under the operating system's temporary directory in `kritikon/review-workspaces/`. They are managed scratch copies; OpenCode does not run inside your working repository.

> **Permission warning:** OpenCode runs with permission auto-approval, as requested, inside the scratch checkout. This skips interactive tool approvals but is not an operating-system sandbox; OpenCode still inherits your user account, network access, and configured credentials. Kritikon's template forbids product edits and direct GitHub posting, and Kritikon itself never posts the draft without the explicit two-step confirmation.

## Configuration

Press `t` in the TUI to configure the refresh timer with either keyboard or mouse:

- Type a whole-number refresh interval in seconds.
- Press `Enter` or `s`, or click **Save**, to validate and persist changes.
- Press `x`, or click **Reset & delete**, then confirm to delete the file and restore defaults.
- Press `Esc` to leave without saving.

Defaults:

```toml
version = 2
refresh_seconds = 30
```

The minimum interval is `5` seconds and there is no configured maximum. Saved values and command-line overrides use the same validation.

Review-request scope is deliberately not configurable. Kritikon always includes direct requests and requests to every visible team you belong to. The selected PR says whether the request came through your username or a specific `organization/team`, and Help lists every monitored team by name.

Configuration is written atomically with user-only file permissions to the native location:

- macOS: `~/Library/Application Support/kritikon/config.toml`
- Linux: `$XDG_CONFIG_HOME/kritikon/config.toml`, or `~/.config/kritikon/config.toml` when `XDG_CONFIG_HOME` is unset or relative.

One-off overrides do not modify the saved file:

```bash
kritikon --refresh-seconds 60
kritikon --refresh-seconds 5
```

Delete the saved file without opening the TUI:

```bash
kritikon --reset-config
```

Check authentication, active settings, monitored teams, config path, and current counts:

```bash
kritikon --check
```

## Review semantics

Kritikon handles every review state exposed by GitHub's GraphQL API:

- `APPROVED`
- `CHANGES REQUESTED`
- `REVIEW REQUIRED`
- `COMMENTED`
- `DISMISSED`
- `PENDING`
- `NO REVIEWS`

GitHub can record several review events from one person. Kritikon shows one effective state per reviewer. A later approval or change request replaces that reviewer's earlier decision; a later informational comment does not erase an existing approval or change request. GitHub's aggregate `reviewDecision` remains the primary PR status.

Outstanding direct and team requests are shown separately from submitted reviews. A re-request can therefore appear alongside your prior review, making the required next action explicit.

## Refresh and coverage

All GitHub work runs in the background. During refresh, the current dashboard stays visible and interactive. Selection is anchored by PR URL and visible screen row, so reordered results do not move focus to another PR. Repeated refresh requests are coalesced instead of launching parallel fetches.

Frequent refreshes query only a lightweight index of open PR IDs and update timestamps. Full comments, reviews, requests, labels, commit authors, and file statistics are cached and fetched once for new or changed PRs; a compact batched status query keeps CI, mergeability, and GitHub's review decision current. Unchanged details are reconciled every 30 minutes when the API budget permits. Every GraphQL operation records its actual remaining budget, skips optional reconciliation below 500 points, and pauses polling below 50 points until GitHub's reported reset while leaving the last complete dashboard on screen.

The **Involved** queue combines GitHub participation search, submitted reviews, and commit-to-PR associations. Each row explains why it appears: `COMMITTED`, `REVIEWED`, `COMMENTED`, `ASSIGNED`, `MENTIONED`, or `PARTICIPATING`.

Commit-only discovery follows associations for the newest 1,000 commits exposed by GitHub Search. After the initial background index, Kritikon checks only the newest commit page every 30 minutes and associates newly seen commits; it reconciles the complete 1,000-commit window every six hours. GitHub Search itself exposes at most 1,000 results per query. Kritikon paginates normal PR searches to that limit and warns when a queue exceeds it.

Private PR visibility exactly matches the active `gh` authentication. Team discovery uses GitHub's authenticated-user teams endpoint; if teams cannot be listed, Kritikon warns without blocking other queues.

## Development

Debug builds include generated scenarios for testing every state without calling GitHub. They use an isolated config at `target/kritikon-dev/config.toml` and add deliberate refresh latency so background behavior is visible. In any populated scenario, `Shift+R` also simulates the review worker's preparing, attachable, and completed phases without launching OpenCode or posting anything.

```bash
cargo run -- --dev-scenario all-states --refresh-seconds 5
cargo run -- --dev-scenario empty
cargo run -- --dev-scenario many
cargo run -- --dev-scenario error
cargo run -- --dev-scenario config-error
```

Run the full local quality gate:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --locked
cargo build --release --locked
```

Pushing a semantic version tag such as `v0.3.0` starts the release workflow. It builds native archives for Apple Silicon macOS, x86_64 Linux, and ARM64 Linux, generates SHA-256 checksums, and publishes a GitHub Release. Changes to the static site deploy automatically to GitHub Pages from `main`.

## License

Kritikon is open-source software licensed under the [Apache License 2.0](LICENSE).
