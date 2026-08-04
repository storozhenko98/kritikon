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
KRITIKON_INSTALL_DIR="$HOME/bin" KRITIKON_VERSION=0.3.2 sh install-kritikon.sh
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
| Expand/collapse details | `d` or `Esc`; arrows/PgUp/PgDn scroll | Wheel/trackpad |
| Configure refresh timer | `t` | Click editor controls |
| Refresh now | `r` | — |
| Full state legend | `?` | — |
| Quit | `q` or `Ctrl+C` | — |

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

Debug builds include generated scenarios for testing every state without calling GitHub. They use an isolated config at `target/kritikon-dev/config.toml` and add deliberate refresh latency so background behavior is visible.

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
