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

- Rust 1.75+ (`rustup install stable`)
- Node.js 22+ and npm 10+
- Docker and Docker Compose
- `jq` (for scripts)
- Optional: Xcode 15+ (iOS SDK), Android Studio (Android SDK)

**Clone and bootstrap:**

```bash
git clone https://github.com/matrixmedia/matrixmedia.git
cd matrixmedia

# Start local dev stack (LiveKit, coturn, MinIO, Synapse)
bash scripts/dev.sh up

# Build Rust workspace
cargo build

# Install web dependencies
cd web/packages/mm-widget && npm ci && cd -
cd web/packages/mm-dashboard && npm ci && cd -
cd web/packages/mm-viewer && npm ci && cd -
```

**Run the server:**

```bash
cargo run -p mm-server
```

**Run the widget dev server:**

```bash
cd web/packages/mm-widget && npm run dev
```

See `docs/quickstart.md` for a more detailed walkthrough.

## Code Style

### Rust

- Format with `cargo fmt --all` before every commit.
- Lint with `cargo clippy --all-targets --all-features -- -D warnings`.
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
# Rust tests
cargo test --workspace

# Web package tests
cd web/packages/mm-widget && npm test
cd web/packages/mm-dashboard && npm test
cd web/packages/mm-viewer && npm test

# End-to-end tests (requires running dev stack)
bash scripts/e2e-test.sh
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

## Contributor License Agreement (CLA)

MatrixMedia is dual-licensed under **AGPL-3.0** and a **Commercial License**.

To accept contributions while preserving the commercial licensing option, all
contributors must sign our Contributor License Agreement (CLA) before their
first pull request is merged.

- **Individual contributors:** sign the Individual CLA.
- **Corporate contributors:** have an authorized representative sign the Corporate CLA.

The CLA bot will automatically prompt you on your first PR. The CLA grants the
MatrixMedia project the rights to relicense your contribution under the commercial
license, while you retain copyright to your work.

See `docs/CLA.md` for the full text and FAQ.

## Reporting Security Vulnerabilities

**Do not report security issues via public GitHub issues.** Email
`security@matrixmedia.io` with a description of the vulnerability and steps to
reproduce. See `SECURITY.md` for the full disclosure policy.

## Getting Help

- **Questions:** open a GitHub Discussion
- **Bugs:** open a GitHub Issue with the bug template
- **Feature requests:** open a GitHub Issue with the feature template
- **Chat:** `#matrixmedia:matrix.org` on Matrix

---

Thank you for helping make MatrixMedia better!
