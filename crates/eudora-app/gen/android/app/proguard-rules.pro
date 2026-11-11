# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# rustls-platform-verifier's Java half (Android's certificate check, called from Rust through JNI,
# which the shrinker can't see): kept whole, as its README says
-keep, includedescriptorclasses class org.rustls.platformverifier.** { *; }

# Eudora's own Kotlin called from (or calling) Rust: EudoraNative's native method, the worker
-keep class net.planet.eudora27.EudoraNative { *; }
-keep class net.planet.eudora27.CheckWorker { *; }
-keep class net.planet.eudora27.SignInService { *; }
-keep class net.planet.eudora27.SecretBox { *; }
-keep class net.planet.eudora27.DocumentPicker { *; }

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
# -keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
# }

# Uncomment this to preserve the line number information for
# debugging stack traces.
# -keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
# -renamesourcefileattribute SourceFile