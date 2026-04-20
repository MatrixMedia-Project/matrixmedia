// Gradle build for the MatrixMedia JVM SDK.
//
// Final shape (TODO for the eventual Rust dev to wire up):
//
//   1. Run `cargo build --release` (or zigbuild for cross) against
//      ../ffi for every supported triple.
//   2. Run `uniffi-bindgen-kotlin` against the produced .so/.dll/.dylib +
//      matrix_sdk_ffi.udl, generating Kotlin sources into
//      `${buildDir}/generated/uniffi/main/kotlin`.
//   3. Copy each native artifact into
//      `src/main/resources/{os}-{arch}/` so JNA / java.lang.System.load can
//      find it at runtime.
//   4. Package the whole thing as a single fat .jar.
//
// Reference pattern (do NOT vendor): the Mages project (AGPL,
// mlm-games/Mages) wires its `core/` Cargo workspace into a Kotlin JAR via
// a custom Gradle task graph. The pieces worth copying conceptually are:
//
//   - `tasks.register<Exec>("cargoBuild") { ... }` per triple
//   - a `copy { from(...) into(...) }` task that lays native libs into
//     resources
//   - `tasks.named<Jar>("jar") { dependsOn("copyNatives") }`
//
// See docs/BUILD.md for the prose walkthrough.

plugins {
    kotlin("jvm") version "2.0.0"
    `java-library`
    `maven-publish`
}

group = "dev.matrixmedia"
version = "0.1.0-SNAPSHOT"

java {
    toolchain {
        languageVersion.set(JavaLanguageVersion.of(17))
    }
    withSourcesJar()
}

kotlin {
    jvmToolchain(17)
}

dependencies {
    // JNA is the runtime dependency uniffi-generated Kotlin uses to load
    // the native lib out of the JAR's resources/.
    api("net.java.dev.jna:jna:5.14.0")

    // Coroutines — uniffi async functions surface as `suspend fun`.
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.1")

    testImplementation(kotlin("test"))
    testImplementation("org.junit.jupiter:junit-jupiter:5.10.2")
}

tasks.test {
    useJUnitPlatform()
}

// ---------------------------------------------------------------------------
// Native build wiring — STUB
// ---------------------------------------------------------------------------
//
// TODO(rust-dev): implement the four phases below. Each is a placeholder
// that currently does nothing useful. Wire them up after matrix-sdk-ffi is
// a real dependency in ../ffi/Cargo.toml.

val rustCrateDir = file("../ffi")
val cargoWorkspaceDir = file("..")

// Map: gradle-task-suffix -> rust target triple
val supportedTriples = mapOf(
    "WindowsX64" to "x86_64-pc-windows-msvc",
    "LinuxX64"   to "x86_64-unknown-linux-gnu",
    "LinuxArm64" to "aarch64-unknown-linux-gnu"
    // TODO(rust-dev): add macOS triples once the .dylib path is exercised.
)

// Phase 1: cargo build per triple.
supportedTriples.forEach { (suffix, triple) ->
    tasks.register<Exec>("cargoBuild$suffix") {
        group = "build"
        description = "Cross-builds the FFI crate for $triple via cargo zigbuild."
        workingDir = cargoWorkspaceDir
        // TODO(rust-dev): swap to `cargo zigbuild` once toolchains exist.
        commandLine("cargo", "zigbuild", "--release", "--target", triple, "-p", "matrix-mm-ffi")
        // Make this opt-in for now; do not break `gradle jar` while scaffolding.
        enabled = false
    }
}

// Phase 2: uniffi-bindgen-kotlin -> generated sources.
tasks.register("uniffiBindgenKotlin") {
    group = "build"
    description = "Generates Kotlin bindings from matrix_sdk_ffi.udl"
    // TODO(rust-dev): invoke `uniffi-bindgen-kotlin generate ../ffi/matrix_sdk_ffi.udl ...`
    //   - output dir: $buildDir/generated/uniffi/main/kotlin
    //   - then add that dir to sourceSets["main"].kotlin.srcDirs
    enabled = false
}

// Phase 3: copy native libs -> src/main/resources/{os}-{arch}/
tasks.register("copyNativeLibs") {
    group = "build"
    description = "Lays cross-built native libs into resources/{os}-{arch}/"
    // TODO(rust-dev): for each triple, copy
    //   ../target/<triple>/release/{libmatrix_mm_ffi.so | .dll | .dylib}
    // into src/main/resources/{linux-x86-64 | win32-x86-64 | linux-aarch64}/
    // matching JNA's resource path convention.
    enabled = false
}

// Phase 4: assemble the fat jar.
tasks.named<Jar>("jar") {
    // dependsOn("copyNativeLibs", "uniffiBindgenKotlin")  // TODO(rust-dev): enable
    archiveBaseName.set("matrixmedia-jvm")
}

publishing {
    publications {
        create<MavenPublication>("maven") {
            from(components["java"])
            artifactId = "matrixmedia-jvm"
        }
    }
}
