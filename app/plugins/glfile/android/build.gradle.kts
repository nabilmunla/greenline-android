// Android library for tauri-plugin-glfile. Not a standalone Gradle project: tauri-build includes it
// in app/src-tauri/gen/android as `:tauri-plugin-glfile` (see ../build.rs), next to `:tauri-android`.
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.greenline.glfile"
    compileSdk = 37

    defaultConfig {
        minSdk = 30
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_1_8
    }
}

dependencies {
    // Same version the app already uses (ActivityResult for the picker callback).
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation(project(":tauri-android"))
    // RecentsCoreTest (JVM): `./gradlew :tauri-plugin-glfile:testDebugUnitTest` from gen/android.
    testImplementation("junit:junit:4.13.2")
}
