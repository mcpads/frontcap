#!/bin/bash
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${1:?Pass a fresh output directory}
SDK=${ANDROID_HOME:-$HOME/Library/Android/sdk}
BT="$SDK/build-tools/35.0.0"
JAR="$SDK/platforms/android-35/android.jar"
mkdir "$OUT"
mkdir "$OUT/classes" "$OUT/dex"
javac -source 11 -target 11 -classpath "$JAR" -d "$OUT/classes" "$HERE/TouchInput.java" "$HERE/InputFiles.java"
"$BT/d8" --min-api 30 --lib "$JAR" --output "$OUT/dex" "$OUT"/classes/dev/frontcap/input/*.class
(cd "$OUT/dex" && zip -q ../touch-input.jar classes.dex)
