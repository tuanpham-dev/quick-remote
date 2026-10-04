#!/bin/sh
# Builds the Android app: the Rust core (qr-ffi) for arm64 and x86_64, then
# the APK. Needs the Android SDK/NDK (ANDROID_HOME, default ~/Android/sdk),
# a JDK 17 (JAVA_HOME) and cargo-ndk.
set -eu
cd "$(dirname "$0")/.."
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/sdk}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$(ls -d "$ANDROID_HOME"/ndk/* | tail -1)}"
cargo ndk -t arm64-v8a -t x86_64 -o android/app/src/main/jniLibs build --release -p qr-ffi
cd android && ./gradlew assembleDebug
ls -la app/build/outputs/apk/debug/
