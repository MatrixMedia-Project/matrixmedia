# Contributing to MatrixMedia

Thank you for your interest in contributing to MatrixMedia! This document outlines the
process for contributing code, documentation, and other improvements to the project.

## Code of Conduct

This project follows the [Contributor Covenant Code of Conduct](https://www.contributor-covenant.org/version/2/1/code_of_conduct/).
By participating, you are expected to uphold this code. Please report unacceptable
behavior to `conduct@matrixmedia.io`.

## Getting Started

### Development Environment Setup

**Prerequisites:**

| Tool | Version | Purpose |
|---|---|---|
| Rust | pinned by `rust-toolchain.toml` | Backend. rustup reads the pin automatically — you do not pick a version. |
| Node.js | pinned by `.nvmrc` (`nvm use`) | Web packages |
| npm | ships with Node | JavaScript package manager. **Not pnpm** — the repo has a `package-lock.json`. |
| Docker + Compose | 24+ | Dev infrastructure (Synapse, LiveKit, coturn, MinIO) |
| `jq` | any | Script utilities |
| `just` | 1.x | Task runner (`brew install just`). See the root `justfile`. |

The toolchains are PINNED, not "minimum". Unpinned, `stable` means "whatever shipped this
week", and the codebase already depends on recent stabilisations (edition 2024, `LazyLock`,
`is_multiple_of`) — an older stable does not warn, it fails to build.

Optional:
- Xcode 15+ (iOS SDK development)
- Android Studio (Android SDK development)
- `cargo-watch` (auto-reload: `cargo install cargo-watch`)

**Clone and bootstrap:**

```bash
git clone https://github.com/matrixmedia/matrixmedia.git
cd matrixmedia

# Start local dev stack (LiveKit, coturn, MinIO, Synapse) + build and run mm-core
bash scripts/dev.sh

# Or start infrastructure only, then build manually:
cd infra/docker && docker compose up -d && cd -
cargo build --all

# Install web dependencies
cd web && npm install && cd -
```

**Run the server:**

```bash
# Two subcommands available:
cargo run -p mm-server -- serve     # Start the server
cargo run -p mm-server -- migrate   # Run database migrations only
```

This starts the client/widget API on `:6167`, admin API on `:6168`, and Prometheus metrics on `:9090`.

**Run the widget dev server:**

```bash
cd web/packages/mm-widget && npm run dev
```

See `docs/quickstart.md` for a more detailed walkthrough.

## Code Style

### Rust

- Format with `cargo fmt --all` before every commit.
- Lint with `cargo clippy --all-targets`.

  **Honesty about the lint bar:** CI does NOT currently enforce `cargo fmt --check` or
  `clippy -D warnings`, and the tree does not pass `-D warnings` today. Do not let that stop
  you contributing, and do not feel obliged to fix unrelated warnings in your PR — but do
  not ADD new ones in code you touch. (This file used to claim a "zero warnings policy" the
  project neither met nor enforced, which is worse than admitting the gap.)
- Prefer small, focused functions; document public APIs with rustdoc.
- Avoid `unwrap()` / `expect()` in library code; return `Result` instead.

### TypeScript / JavaScript

- Format with `prettier --write .` (config is checked in).
- Lint with `npm run lint` in each package.
- Use TypeScript strict mode; avoid `any`.
- Prefer named exports over default exports.

### Swift (iOS SDK)

- Format with `swiftformat .` (config in `sdks/ios/.swiftformat`).
- Lint with `swiftlint` if available.

### Kotlin (Android SDK)

- Format with `ktlint` (configured via Gradle).
- Follow the Kotlin coding conventions.

## Testing Requirements

All contributions must include tests and all existing tests must pass.

**Run the full test suite:**

```bash
# Rust tests (137 tests across 6 crates)
cargo test --all

# Rust tests for a specific crate
cargo test -p mm-core
cargo test -p mm-api
cargo test -p mm-db

# Web package tests
cd web/packages/mm-widget && npm test
cd web/packages/mm-dashboard && npm test
cd web/packages/mm-viewer && npm test

# End-to-end tests (13 steps, requires running dev stack)
bash scripts/e2e-test.sh

# Load test (50 concurrent viewers)
bash scripts/load-test.sh
```

**Pre-commit checks (all must pass):**

```bash
cargo fmt --check                        # Formatting (not enforced by CI yet)
cargo clippy --all                       # Linting (NOT -D warnings: the tree does not pass it yet)
cargo test --all                         # All 137 tests
```

**Coverage expectations:**

- New features: unit tests + at least one integration test.
- Bug fixes: a regression test that fails without the fix.
- Public API changes: updates to `contracts/api/*.yaml` + schema tests.
- Protocol/wire format changes: updated test vectors.

## Pull Request Process

1. **Fork the repo** and create a feature branch off `main`:
   ```bash
   git checkout -b feat/my-feature
   ```

2. **Make your changes**, following the code style and testing requirements above.

3. **Run the pre-flight checks locally:**
   ```bash
   cargo fmt --all --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --workspace
   ```

4. **Commit** using conventional commits (see below).

5. **Push your branch** and open a pull request against `main`.

6. **Fill out the PR template**, including:
   - Summary of changes
   - Test plan
   - Breaking change notes (if any)
   - Related issues (`Closes #123`)

7. **Respond to review feedback.** Maintainers aim to review within 2 business days.

8. **Squash and merge.** Once approved and CI is green, a maintainer will merge.

### What makes a good PR

- Keep PRs focused and small (< 500 lines of diff when possible).
- One logical change per PR.
- Update documentation alongside code changes.
- Update `CHANGELOG.md` under `[Unreleased]`.

## Commit Message Conventions

This project uses [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).

**Format:**

```
<type>(<scope>): <subject>

<body>

<footer>
```

**Types:**

- `feat`: A new feature
- `fix`: A bug fix
- `docs`: Documentation only changes
- `style`: Formatting, missing semicolons, etc.
- `refactor`: Code change that neither fixes a bug nor adds a feature
- `perf`: A code change that improves performance
- `test`: Adding or updating tests
- `build`: Changes affecting the build system or dependencies
- `ci`: Changes to CI configuration
- `chore`: Other changes that don't modify src or test files
- `revert`: Reverts a previous commit

**Scopes (examples):**

`server`, `widget`, `dashboard`, `viewer`, `sdk-ios`, `sdk-android`, `docs`,
`infra`, `contracts`, `e2ee`, `federation`, `recording`

**Examples:**

```
feat(widget): add screen-share toggle button

fix(server): prevent token replay when jti TTL expires early

docs(quickstart): clarify MinIO credential setup

Closes #142
```

**Breaking changes** must include `!` after the type/scope and a `BREAKING CHANGE:`
footer:

```
feat(api)!: rename /rooms/:id/join to /rooms/:id/connect

BREAKING CHANGE: clients must update to the new endpoint path.
```

## Licensing and sign-off

**MatrixMedia is Apache-2.0.** See `LICENSE`. There is no CLA, and no commercial
relicensing: your contribution stays under the same licence as the rest of the project, and
you keep your copyright.

> This file previously demanded a Contributor License Agreement, describing the project as
> "dual-licensed under **AGPL-3.0** and a **Commercial License**", and granting the project
> the right to relicense your work commercially. That contradicted `LICENSE`, `README.md`
> and `Cargo.toml`, all of which say Apache-2.0, and it referenced a `docs/CLA.md` that does
> not exist. A contributor reading it had to choose between signing away rights under a
> licence the project does not use, or walking away. It was removed, not softened.

Instead we use the **Developer Certificate of Origin** (DCO) — the same lightweight
mechanism the Linux kernel and Git use. You certify that you wrote the patch, or otherwise
have the right to submit it under Apache-2.0, by signing off your commits:

```bash
git commit -s -m "fix(api): ..."     # -s appends the Signed-off-by trailer
```

That trailer is the whole of it. No forms, no bot, no account.

The full DCO text is at <https://developercertificate.org/>.

## Reporting Security Vulnerabilities

**Do not report security issues via public GitHub issues.** Email
`security@matrixmedia.io` with a description of the vulnerability and steps to
reproduce. See `SECURITY.md` for the full disclosure policy.

## Issue Templates

When opening an issue, use the appropriate template:

- **Bug Report** (`bug_report.md`): Steps to reproduce, expected vs. actual behavior, environment details (OS, Rust version, Docker version, homeserver type).
- **Feature Request** (`feature_request.md`): Use case description, proposed solution, alternatives considered.
- **Security Vulnerability**: Do **not** use public issues. Email `security@matrixmedia.io` instead (see `SECURITY.md`).

For questions and general discussion, consider the Matrix room first -- you will often get a faster response.

## Project Structure

```
matrixmedia/
  crates/           # 6 Rust crates (mm-core, mm-api, mm-db, mm-matrix, mm-sfu, mm-server)
  web/packages/     # 3 web apps (mm-widget, mm-dashboard, mm-viewer)
  sdks/             # Mobile SDKs (iOS Swift, Android Kotlin, Flutter Dart)
  contracts/        # OpenAPI spec + Matrix event schemas
  infra/            # Docker, Helm (14 templates), Grafana (15 panels), Prometheus (8 alerts)
  scripts/          # Dev, E2E test, load test scripts
  docs/             # 27+ documentation files including MSC drafts
```

See [IMPLEMENTATION.md](IMPLEMENTATION.md) for a comprehensive technical overview of the entire system.

## Getting Help

- **Chat:** [#matrixmedia:matrix.org](https://matrix.to/#/#matrixmedia:matrix.org) on Matrix
- **Questions:** open a GitHub Discussion
- **Bugs:** open a GitHub Issue with the bug template
- **Feature requests:** open a GitHub Issue with the feature template
- **Documentation:** Start with [docs/quickstart.md](docs/quickstart.md) and [IMPLEMENTATION.md](IMPLEMENTATION.md)

---

Thank you for helping make MatrixMedia better!
