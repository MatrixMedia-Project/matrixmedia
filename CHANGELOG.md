# Changelog

All notable changes to MatrixMedia will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-04-04

### Added

#### Phase 0 -- Foundation
- OpenAPI spec with 17 endpoints
- 3 Matrix event schemas
- Cargo workspace with 6 crates
- Docker Compose with LiveKit, coturn, MinIO, Synapse

#### Phase 1a -- Core Backend + Widget
- Rust backend with SQLite, JWT auth, LiveKit SFU integration
- SolidJS widget with LiveKit audio support
- 78 tests

#### Phase 1b -- Platform
- React admin dashboard (6 pages)
- React standalone web viewer
- iOS SDK (Swift Package)
- Android SDK (Kotlin)
- Helm chart
- 3 MSC drafts

#### Phase 2 -- Video + CDN + MSCs
- Video and screen share streaming
- S3 storage adapter (AWS S3, R2, MinIO)
- CDN signed URLs
- LiveKit Egress for HLS
- 113 tests

#### Phase 3 -- VoD + Recording
- Recording pipeline (Egress -> S3 -> MXC)
- HLS playback in widget, viewer, SDKs
- Admin recording management UI
- 116 tests

#### Phase 4 -- E2EE
- End-to-end encryption via LiveKit Insertable Streams
- Matrix state event key distribution
- AES-GCM-256 shared room keys
- Key rotation API
- 126 tests

#### Phase 5 -- Federation (v1)
- Federated OpenID validation
- Federation allow/deny lists
- .well-known/matrix/matrixmedia service discovery
- 137 tests

### Security
- AGPL-3.0 + Commercial dual license
- Security audit passed (see docs/security-audit.md)

[Unreleased]: https://github.com/matrixmedia/matrixmedia/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/matrixmedia/matrixmedia/releases/tag/v0.1.0
