pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
        // LiveKit Android pulls `audioswitch` from JitPack — required for
        // `compileDebugKotlin` to resolve transitive deps.
        maven { url = uri("https://jitpack.io") }
    }
}
rootProject.name = "matrixmedia-android"
include(":mm-sdk")
include(":example-app")
