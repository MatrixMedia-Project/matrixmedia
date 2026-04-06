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

## Version Strategy

- **Semver:** MAJOR.MINOR.PATCH
- Phase 1: 0.1.x (pre-release, API may change)
- Phase 2: 0.2.x (video support added)
- 1.0.0: When API is stable and E2EE is implemented
- Breaking changes only in MINOR bumps during 0.x
- Both SDKs share version numbers with the backend
