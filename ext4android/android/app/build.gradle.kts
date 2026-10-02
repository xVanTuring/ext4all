import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.compose.compiler)
}

// NDK for cargo-ndk (scripts/build-rust.sh) and for AGP to strip the library
val ndkVersionUsed = "30.0.16248370"

android {
    namespace = "tech.xvanturing.ext4android"
    // Compose 1.12 needs 37 or later; the platform is installed by
    // scripts/setup-toolchain.sh
    compileSdk {
        version = release(37) {
            minorApiLevel = 2
        }
    }
    ndkVersion = ndkVersionUsed

    defaultConfig {
        applicationId = "tech.xvanturing.ext4android"
        minSdk = 30
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        ndk {
            // arm64 devices, x86_64 emulator
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
    }
}

dependencies {
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.material3)
}

// libext4android.so is built from rust/ by cargo-ndk into src/main/jniLibs.
// Android Studio does not read the shell profile, so the NDK path is passed
// from here (sdk.dir in local.properties, else ANDROID_HOME).
val sdkDir: String = run {
    val props = Properties()
    val local = rootProject.file("local.properties")
    if (local.exists()) {
        local.inputStream().use(props::load)
    }
    props.getProperty("sdk.dir")
        ?: System.getenv("ANDROID_HOME")
        ?: "${System.getProperty("user.home")}/Library/Android/sdk"
}

val buildRust = tasks.register<Exec>("buildRust") {
    group = "build"
    description = "Builds libext4android.so with cargo-ndk"
    workingDir = rootDir.parentFile
    environment("ANDROID_NDK_HOME", "$sdkDir/ndk/$ndkVersionUsed")
    commandLine("/bin/bash", "scripts/build-rust.sh")
}

tasks.named("preBuild") {
    dependsOn(buildRust)
}
