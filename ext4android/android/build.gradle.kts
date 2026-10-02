plugins {
    alias(libs.plugins.android.application) apply false
    // Also puts Kotlin Gradle plugin 2.4 on the build classpath, in place of
    // the older version that AGP's built-in Kotlin depends on: the Compose
    // compiler plugin must match the Kotlin version.
    alias(libs.plugins.compose.compiler) apply false
}
