# SDK Publishing Guide

## iOS SDK (Swift Package Manager)

### Prerequisites
- Xcode 15.2+
- GitHub account with push access to the SDK repo

### Publishing Steps

1. **Tag the release:**
   ```bash
   cd sdks/ios
   git tag 0.1.0
   git push origin 0.1.0
   ```

2. **Users add the dependency in Xcode:**
   - File -> Add Package Dependencies
   - Enter: `https://github.com/matrixmedia/matrixmedia` (monorepo root)
   - Select "MatrixMediaSDK" product

3. **Or via Package.swift:**
   ```swift
   .package(url: "https://github.com/matrixmedia/matrixmedia", from: "0.1.0")
   ```
   Note: SPM will resolve the package from the monorepo root.

### Local Development
```bash
# Add as local package in Xcode:
# Drag the sdks/ios/ directory into your Xcode project
# Or use .package(path: "../matrixmedia/sdks/ios")
```

---

## Android SDK (Maven Central)

### Prerequisites
- JDK 17
- Sonatype OSSRH account (for Maven Central)
- GPG signing key

### Publishing Steps

1. **Configure credentials** in `~/.gradle/gradle.properties`:
   ```properties
   ossrhUsername=your-username
   ossrhPassword=your-password
   signing.keyId=your-key-id
   signing.password=your-key-password
   signing.secretKeyRingFile=/path/to/secring.gpg
   ```

2. **Build and publish:**
   ```bash
   cd sdks/android
   ./gradlew :mm-sdk:publishReleasePublicationToMavenCentralRepository
   ```

3. **Users add the dependency:**
   ```kotlin
   // build.gradle.kts
   dependencies {
       implementation("com.matrixmedia:mm-sdk:0.1.0")
   }
   ```

### Local Development
```bash
# Publish to local Maven:
./gradlew :mm-sdk:publishToMavenLocal

# Use in another project:
# repositories {
#     mavenLocal()
# }
```

### JitPack (alternative, no account needed)
Users can add JitPack as a repository and reference the GitHub repo directly:
```kotlin
repositories {
    maven { url = uri("https://jitpack.io") }
}
dependencies {
    implementation("com.github.matrixmedia:matrixmedia:0.1.0")
}
```

---

## Web SDK (npm)

The web SDK is three packages under `web/packages/`, published to the public
**`@matrixmedia`** npm scope (Apache-2.0):

- `@matrixmedia/client` — framework-agnostic REST client (`.`) + WebRTC
  viewer/publisher (`./webrtc`).
- `@matrixmedia/widget` — the embeddable `<mm-stream>` custom element.
- `@matrixmedia/react` — React provider, hooks, and components.

See [docs/web-sdk/](web-sdk/getting-started.md) for consumer docs and
[docs/web-sdk/architecture.md](web-sdk/architecture.md) for build internals.

### Prerequisites
- Node 20+
- The owner-provisioned `@matrixmedia` npm org (scope must exist and grant the
  CI identity publish rights).
- An `NPM_TOKEN` repo secret (automation token with publish scope).

### Publishing Steps

Releases are driven by **Changesets** + a GitHub Action (added in Task 7); the
day-to-day flow is:

1. **Add a changeset** with your PR describing the bump per package:
   ```bash
   npx changeset            # pick packages + bump level, write a summary
   ```
2. **Merge to the default branch.** The release Action runs
   `changeset version` (applies bumps + updates changelogs) and then
   `changeset publish` to npm.
3. **Provenance.** Publishing uses `npm publish --provenance` (the Action runs
   with `id-token: write`) so each package gets a signed provenance attestation.

The Action is gated on the `NPM_TOKEN` secret and the `@matrixmedia` org being
provisioned; without both, publishing is a no-op/failure by design.

### Version Strategy (web)
- Independent **`0.1.0`** version line, **not** tied to mm-core's version
  (unlike the iOS/Android SDKs below). See
  [ADR-0010](adr/0010-web-sdk-packaging.md).

---

## Version Strategy

- **Semver:** MAJOR.MINOR.PATCH
- Phase 1: 0.1.x (pre-release, API may change)
- Phase 2: 0.2.x (video support added)
- 1.0.0: When API is stable and E2EE is implemented
- Breaking changes only in MINOR bumps during 0.x
- The iOS/Android SDKs share version numbers with the backend; the web SDK
  versions independently (see the Web SDK section above).
