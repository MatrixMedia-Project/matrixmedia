# MatrixMedia Security Audit

**Date:** 2026-04-04
**Scope:** All Rust crates, web packages (mm-widget, mm-dashboard, mm-viewer), iOS/Android SDKs
**Tools used:** `cargo audit` v0.22.1, `cargo deny` v0.19.0, `npm audit`, manual secrets grep

## Summary

Overall security posture is **acceptable for development**, with **2 known vulnerabilities** in transitive Rust dependencies that require monitoring. No web package vulnerabilities, no hardcoded secrets in production code, and no sensitive data logged. Secrets handling follows best practices (env-var loading, `_FROM_FILE` suffix support, `skip_serializing` on secret fields).

**Action items:**
1. (Blocking-ish) Track upstream fixes for `rsa 0.9.10` (Marvin Attack) — transitive via `livekit-api` and `sqlx-mysql`.
2. (Easy fix) Upgrade `prometheus` to a version that pulls `protobuf >= 3.7.2` or pin `protobuf` via `[patch]`.
3. Add `publish = false` to internal workspace crates to clean up cargo-deny output.
4. Add `security-audit` job to CI (see below).

---

## Rust Dependencies (cargo audit)

`cargo audit` scanned **476 crate dependencies** against **1026 security advisories**.

### Vulnerabilities

#### RUSTSEC-2024-0437 — protobuf stack overflow (medium)

| Field | Value |
|---|---|
| Crate | `protobuf 2.28.0` |
| Title | Crash due to uncontrolled recursion in protobuf crate |
| Severity | Medium (DoS) |
| Solution | Upgrade to `>=3.7.2` |
| Path | `protobuf 2.28.0` <- `prometheus 0.13.4` (via all mm-* crates) |

Pulled in as a transitive dependency of `prometheus`. Upgrade `prometheus` to a version that uses `protobuf >= 3.7.2`, or add a `[patch.crates-io]` override.

#### RUSTSEC-2023-0071 — RSA Marvin Attack (medium, 5.9 CVSS)

| Field | Value |
|---|---|
| Crate | `rsa 0.9.10` |
| Title | Marvin Attack: potential key recovery through timing sidechannels |
| Severity | Medium (5.9) |
| Solution | **No fix available upstream** |
| Path 1 | `rsa 0.9.10` <- `sqlx-mysql 0.8.6` <- `sqlx 0.8.6` <- `mm-db` |
| Path 2 | `rsa 0.9.10` <- `jsonwebtoken 10.3.0` <- `livekit-api 0.4.18` <- `mm-sfu` |

**Mitigation:** Marvin Attack exploits network-observable timing sidechannels during RSA decryption. MatrixMedia's exposure is limited:
- MySQL is not used in dev (SQLite default, Postgres for prod); the `sqlx-mysql` path is unreachable at runtime if MySQL is not configured.
- `livekit-api` uses RSA for JWT verification — an attacker would need to observe decryption timing over the network to exploit.
- Track fix upstream: https://github.com/RustCrypto/RSA/issues/19

### Warnings

#### RUSTSEC-2026-0002 — `lru 0.12.5` unsoundness

`IterMut` violates Stacked Borrows by invalidating internal pointer. Transitive via `aws-sdk-s3 1.119.0` -> `mm-core`. Informational only.

---

## License Compliance (cargo deny)

**Result:** `licenses ok` (after allowing `AGPL-3.0-or-later` for internal crates).

All dependency licenses are within the allow list: MIT, Apache-2.0 (including LLVM exception), BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, plus the project's own AGPL-3.0-or-later.

Note: `MPL-2.0`, `CC0-1.0`, `Unicode-DFS-2016` are in the allow list but were not encountered in the current dependency graph (harmless future-proofing).

### Duplicate dependency warnings (non-blocking)

Multiple versions present (all transitive, forced by upstream crates):
- `base64` (0.21.7, 0.22.1)
- `core-foundation` (0.9.4, 0.10.1)
- `getrandom` (3 versions)
- `hashbrown` (3 versions)
- `heck`, `itertools`, `jsonwebtoken`, `r-efi`, `rand`, `rand_chacha`, `rand_core`, `thiserror`, `thiserror-impl`, `windows-sys` (2 versions each)

None of these indicate a security problem — they're byproducts of a large dep graph. Can be reduced over time via `cargo update` and upgrading major deps.

### Wildcard dependency errors (false positive)

Cargo-deny flags workspace-internal path dependencies (`mm-core`, `mm-db`, `mm-sfu`, `mm-matrix`, `mm-api`) as wildcards. These are not meaningful because the crates are workspace-private. **Fix:** add `publish = false` to the `[package]` section of each internal crate's `Cargo.toml` so cargo-deny recognizes them as non-public.

---

## JavaScript Dependencies (npm audit)

### mm-widget

| Severity | Count |
|---|---|
| Critical | 0 |
| High | 0 |
| Moderate | 0 |
| Low | 0 |
| Info | 0 |

Total dependencies: 138 (18 prod, 119 dev, 53 optional).

### mm-dashboard

| Severity | Count |
|---|---|
| Critical | 0 |
| High | 0 |
| Moderate | 0 |
| Low | 0 |
| Info | 0 |

Total dependencies: 122 (8 prod, 115 dev, 52 optional).

### mm-viewer

| Severity | Count |
|---|---|
| Critical | 0 |
| High | 0 |
| Moderate | 0 |
| Low | 0 |
| Info | 0 |

Total dependencies: 137 (21 prod, 115 dev, 53 optional).

**All web packages are clean.**

---

## Secrets Handling Review

### Config secrets — Good

Secrets are loaded exclusively from environment variables or files (via `_FROM_FILE` suffix), never from committed TOML files. Reviewed in `crates/mm-core/src/config.rs`:

- `jwt_signing_key`, `admin_token`, `matrix.as_token`, `matrix.hs_token`, `sfu.livekit_api_key`, `sfu.livekit_api_secret`, `storage.s3.access_key`, `storage.s3.secret_key`, `cdn.signing_key` — all annotated with `#[serde(default, skip_serializing)]` and loaded from env.
- Env-var overrides log only the variable name being set (e.g. `"Config override: MM_JWT_SIGNING_KEY"`), never the value.
- `.env` is gitignored.
- `.env.example` exists at `infra/docker/.env.example` with clearly-marked dev placeholders (e.g. `dev-signing-key-must-be-at-least-32-bytes!!`).

### Production code grep results — Clean

Scanned all `crates/**/*.rs` for hardcoded secrets. All matches for `password|secret|api_key|token` fall into these legitimate buckets:
- Struct field names (`as_token`, `hs_token`, `api_key`, `admin_token`)
- Function parameter names (`api_key: &str`, `api_secret: &str`)
- Type imports (`AccessToken`, `TokenVerifier`, `SfuTokenClaims`)
- Method calls (`.bearer_auth(&self.as_token)`, `.with_api_key(api_key, api_secret)`)
- Doc comments and log messages mentioning variable names
- Test fixtures in `#[cfg(test)]` modules (e.g. `TEST_KEY`, `TEST_SECRET`, `"test-openid-token-abc123"`)

### Logging hygiene — Good

- Grep for `tracing::*!(...token...)`, `println!`-style macros containing `token|secret|password|api_key` variables: **no matches in prod code**.
- Config `Debug` impl is derived but config is never printed via `{:?}` anywhere in the codebase.
- `info!("Config override: MM_XXXX")` logs the variable name only.

### Findings

- **No hardcoded production secrets found.**
- **No tokens, passwords, or secrets logged.**
- **All test tokens are clearly marked and scoped to `#[cfg(test)]`.**

### Recommendations

- Consider redacting sensitive fields in `Debug` manually (e.g. via `#[derive(Debug)]` + custom `fmt::Debug` or the `secrecy` crate) to defend against accidental future logging. Current risk is low since no `{:?}` of Config is logged.
- Ensure production deployments use the `_FROM_FILE` variants of secret env vars (already supported in `read_env_or_file`).

---

## Known Limitations / Accepted Risks

| Risk | Rationale |
|---|---|
| `rsa 0.9.10` Marvin Attack | No upstream fix. Exposure limited to JWT verification in `livekit-api` and (unused) `sqlx-mysql`. Monitor RustCrypto/RSA#19. |
| `lru 0.12.5` unsoundness | Transitive via `aws-sdk-s3`; stacked-borrows issue only, not a concrete exploit. Track `aws-sdk-s3` updates. |
| Duplicate deps | Non-exploitable; will be reduced naturally as upstream deps upgrade. |
| Internal crate wildcards | False positive; see fix above. |

---

## CI Integration

A weekly `security-audit` job has been added to `.github/workflows/ci.yml`. It runs `cargo audit` and `npm audit --audit-level=high` on all web packages. See CI file for details.

Recommended future additions:
- Add `cargo deny check advisories` to the same job (blocking on new CVEs).
- Automate `cargo update -p protobuf` via Dependabot or Renovate.
- Add Trivy / Grype scanning for Docker images if publishing images.
