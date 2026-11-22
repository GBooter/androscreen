import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// 发布证书不随仓库分发：公开它等于允许任何人签出能覆盖已安装应用的 APK。
// 口令与证书路径从 local.properties 或 ANDROSCREEN_* 环境变量读取；
// 未提供时回退到模块目录下的 androscreen-release.keystore（该文件已被 .gitignore 忽略）。
val localProps = Properties().apply {
    val localFile = rootProject.file("local.properties")
    if (localFile.isFile) localFile.inputStream().use { load(it) }
}

fun signingValue(key: String, env: String): String? =
    localProps.getProperty(key) ?: System.getenv(env)

val releaseKeystore = file(
    signingValue("androscreen.storeFile", "ANDROSCREEN_STORE_FILE")
        ?: "androscreen-release.keystore",
)
val storePasswordValue = signingValue("androscreen.storePassword", "ANDROSCREEN_STORE_PASSWORD")
val keyAliasValue = signingValue("androscreen.keyAlias", "ANDROSCREEN_KEY_ALIAS")
val keyPasswordValue = signingValue("androscreen.keyPassword", "ANDROSCREEN_KEY_PASSWORD")
val canSignRelease = releaseKeystore.isFile &&
    storePasswordValue != null && keyAliasValue != null && keyPasswordValue != null

android {
    namespace = "com.androscreen.app"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.androscreen.app"
        minSdk = 26
        targetSdk = 34
        versionCode = 37
        versionName = "0.4.31"
    }

    signingConfigs {
        // Android 拒绝安装签名变化的更新，所以这把证书必须稳定；
        // 它留在本机，由 local.properties 或环境变量提供口令。
        if (canSignRelease) {
            create("shared") {
                storeFile = releaseKeystore
                storePassword = storePasswordValue
                keyAlias = keyAliasValue
                keyPassword = keyPasswordValue
            }
        }
    }

    testOptions {
        // The protocol codecs log through android.util.Log, and a unit test
        // has no runtime to answer it.
        unitTests.isReturnDefaultValues = true
    }

    buildTypes {
        debug {
            // 有发版证书时才用它；没有就留着 AGP 自己的调试证书。
            // 直接把 findByName 的结果赋给 signingConfig，在没有证书的机器上会是
            // null，连调试签名一起覆盖掉，产出 app-debug-unsigned.apk —— 装不进手机，
            // 也就没法在真机上验证界面。
            signingConfigs.findByName("shared")?.let { signingConfig = it }
        }
        release {
            // The DEB ships this build, so it has to be signed: an unsigned
            // release is written as app-release-unsigned.apk, which packaging
            // never found, and it fell back to the debuggable debug build.
            signingConfig = signingConfigs.findByName("shared")
            isMinifyEnabled = false
        }
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    composeOptions {
        kotlinCompilerExtensionVersion = "1.5.14"
    }

    packaging {
        resources.excludes += "/META-INF/{AL2.0,LGPL2.1}"
    }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2024.06.00"))
    implementation("androidx.activity:activity-compose:1.9.0")
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")

    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
}
