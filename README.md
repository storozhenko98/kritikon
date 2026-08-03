# Review Monitor

A focused terminal dashboard for the two GitHub queues that matter:

1. Open pull requests where you or one of your teams has an outstanding review request.
2. Every open pull request you authored, including drafts and PRs with no reviews.

The list stays compact; the selected PR gets a detailed pane with its complete review breakdown, waiting reviewers, draft/ready state, mergeability, CI rollup, branches, files, line changes, comments, labels, and timestamps. Closed PRs are never queried.

## Run it

Prerequisites:

- [GitHub CLI](https://cli.github.com/) installed and authenticated (`gh auth login`).
- Rust 1.88 or newer to build/install the binary.

From this repository:

```bash
cargo run --release
```

Or install it into Cargo's binary directory and run it from anywhere:

```bash
cargo install --path .
review-monitor
```

Review Monitor reuses the active `gh` account and host. It never reads or stores your token itself. Team review requests use GitHub's authenticated-user teams endpoint; classic tokens need the `read:org`, `repo`, or `user` scope for that endpoint. If teams cannot be listed, the app remains usable and clearly warns that only direct requests are shown.

Check connectivity without starting the full-screen UI:

```bash
cargo run --release -- --check
```

## Controls

| Action | Keyboard | Mouse |
| --- | --- | --- |
| Move | `↑` / `↓`, `j` / `k`, `PgUp` / `PgDn`, `Home` / `End` | Wheel/trackpad |
| Switch view | `1`, `2`, `Tab`, `←`, `→` | Click a tab |
| Open selected PR | `Enter` or `o` | Single-click a PR row |
| Expand/collapse details | `d` or `Esc` to collapse; arrows/PgUp/PgDn scroll | Wheel/trackpad |
| Refresh | `r` | — |
| Full state legend | `?` | — |
| Quit | `q` or `Ctrl+C` | — |

The dashboard refreshes every five minutes by default. Change or disable that interval with:

```bash
review-monitor --refresh-seconds 60
review-monitor --refresh-seconds 0
```

To skip team discovery and show only requests addressed directly to your user:

```bash
review-monitor --no-team-requests
```

## State semantics

Review Monitor handles all states currently exposed by GitHub's GraphQL API:

- `APPROVED`
- `CHANGES REQUESTED`
- `REVIEW REQUIRED`
- `COMMENTED`
- `DISMISSED`
- `PENDING`
- `NO REVIEWS`

GitHub can record several review events from the same person. The reviewer breakdown shows one effective state per reviewer. A later approval or change request replaces that reviewer's earlier decision; a later informational comment does not erase an existing approval/change request. GitHub's aggregate `reviewDecision` remains the primary status shown for the PR.

Outstanding direct and team requests are shown separately from submitted reviews. A re-request can therefore appear alongside your prior review, which is intentional and makes the required next action visible.

## Data and limits

- Search is always scoped with `is:pr is:open`.
- Results are sorted by most recently updated.
- GitHub Search exposes at most 1,000 results for a query. The app paginates to that limit and warns if a queue exceeds it.
- The detail view fetches the latest 100 review events and first 100 outstanding review requests per PR, while still showing GitHub's full event/request totals.
- Private PR visibility is exactly the visibility of the active `gh` authentication.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
