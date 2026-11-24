#!/bin/bash
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${1:?Usage: build.sh new-output-directory}
mkdir -p "$(dirname "$OUT")"
mkdir "$OUT"
OUT=$(cd "$OUT" && pwd)
SDK=$HOME/Library/Android/sdk
BT=$(ls -d "$SDK"/build-tools/* | sort -V | tail -1)
JAR=$SDK/platforms/android-35/android.jar
mkdir "$OUT/classes" "$OUT/dex"
javac -source 11 -target 11 -classpath "$JAR" -d "$OUT/classes" "$HERE/FrameIme.java"
classes=(); while IFS= read -r -d '' file; do classes+=("$file"); done < <(find "$OUT/classes" -name '*.class' -print0)
"$BT/d8" --min-api 30 --lib "$JAR" --output "$OUT/dex" "${classes[@]}"
"$BT/aapt2" compile --dir "$HERE/res" -o "$OUT/res.zip"
"$BT/aapt2" link -o "$OUT/unsigned.apk" --manifest "$HERE/AndroidManifest.xml" -I "$JAR" "$OUT/res.zip"
(cd "$OUT/dex" && zip -q "$OUT/unsigned.apk" classes.dex)
"$BT/zipalign" -p -f 4 "$OUT/unsigned.apk" "$OUT/frame-ime.apk"
keytool -genkeypair -keystore "$OUT/key" -alias fixture -keyalg RSA -keysize 2048 -validity 30 -storepass android -keypass android -dname 'CN=Frame IME' > "$OUT/keytool.log" 2>&1
"$BT/apksigner" sign --ks "$OUT/key" --ks-pass pass:android "$OUT/frame-ime.apk"
"$BT/apksigner" verify "$OUT/frame-ime.apk"
