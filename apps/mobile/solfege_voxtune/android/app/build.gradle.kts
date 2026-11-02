import java.util.Properties

plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

// The Play upload key. `android/key.properties` (never committed) names the
// keystore and its passwords; without it a release build is signed with the
// debug key, which runs on a device but which Play refuses.
val uploadKey = Properties().apply {
    val file = rootProject.file("key.properties")
    if (file.exists()) {
        file.inputStream().use { load(it) }
    }
}

// The app's `.env` (see `.env.example`): KEY=value lines, `#` comments. Only
// PLAY_LICENSE_PUBLIC_KEY is used here; the upload settings are the upload
// script's.
val appEnv: Map<String, String> = rootProject.file("../.env").let { file ->
    if (!file.exists()) {
        emptyMap()
    } else {
        file.readLines()
            .map { it.trim() }
            .filter { it.isNotEmpty() && !it.startsWith("#") && it.contains('=') }
            .associate { line ->
                val (key, value) = line.split('=', limit = 2)
                key.trim() to value.trim().removeSurrounding("\"")
            }
    }
}

android {
    namespace = "studio.futureboard.voxtune"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = flutter.ndkVersion

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        // The id the app is registered under on Google Play; it can never
        // change once a build has been published.
        applicationId = "studio.futureboard.voxtune"
        minSdk = flutter.minSdkVersion
        targetSdk = flutter.targetSdkVersion
        // From `version:` in pubspec.yaml (name+code).
        versionCode = flutter.versionCode
        versionName = flutter.versionName
        // libcanto_dsp.so is built for these two only (see README), so a
        // 32-bit device must not be offered an app that cannot load it.
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
        // Google Play licensing key; empty turns the purchase check off.
        buildConfigField(
            "String",
            "PLAY_LICENSE_PUBLIC_KEY",
            "\"${appEnv["PLAY_LICENSE_PUBLIC_KEY"].orEmpty()}\"",
        )
    }

    buildFeatures {
        buildConfig = true
        // ILicensingService / ILicenseResultListener (src/main/aidl).
        aidl = true
    }

    signingConfigs {
        if (uploadKey.isNotEmpty()) {
            create("upload") {
                storeFile = file(uploadKey.getProperty("storeFile"))
                storePassword = uploadKey.getProperty("storePassword")
                keyAlias = uploadKey.getProperty("keyAlias")
                keyPassword = uploadKey.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            signingConfig = signingConfigs.findByName("upload")
                ?: signingConfigs.getByName("debug")
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}
