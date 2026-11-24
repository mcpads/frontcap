#!/bin/bash
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${1:?Usage: build.sh new-output-directory}
MODE=${2:-observe}
FLAGS=()
if [[ "$MODE" == file-worker || "$MODE" == hold-worker || "$MODE" == control-worker || "$MODE" == sequential-worker ]]; then
  FLAGS+=(-DFRONTCAP_SPEECH_FILE_WORKER)
  if [[ "$MODE" != file-worker ]]; then FLAGS+=(-DFRONTCAP_SPEECH_HOLD); fi
  if [[ "$MODE" == control-worker || "$MODE" == sequential-worker ]]; then FLAGS+=(-DFRONTCAP_SPEECH_CONTROL); fi
  if [[ "$MODE" == sequential-worker ]]; then FLAGS+=(-DFRONTCAP_SPEECH_SEQUENTIAL); fi
elif [[ "$MODE" == flow ]]; then
  FLAGS+=(-DFRONTCAP_SPEECH_FLOW)
elif [[ "$MODE" != observe ]]; then
  echo "Expected observe, flow, file-worker, hold-worker, control-worker or sequential-worker" >&2
  exit 1
fi
mkdir -p "$(dirname "$OUT")"
mkdir "$OUT"
OUT=$(cd "$OUT" && pwd)
SDK=$HOME/Library/Android/sdk
NDK=$(ls -d "$SDK"/ndk/* | sort -V | tail -1)
JAVA_ROOT=${JAVA_HOME:-$(/usr/libexec/java_home)}
OBJECTS=()
if [[ "$MODE" == control-worker || "$MODE" == sequential-worker ]]; then
  "$NDK"/toolchains/llvm/prebuilt/*/bin/aarch64-linux-android30-clang \
    -c -fPIC -std=c11 -Wall -Wextra -Werror \
    "$HERE/../webview-timeline/speech_progress.c" -o "$OUT/speech_progress.o"
  OBJECTS+=("$OUT/speech_progress.o")
fi
"$NDK"/toolchains/llvm/prebuilt/*/bin/aarch64-linux-android30-clang++ \
  -static-libstdc++ -shared -fPIC -O1 -std=c++17 -Wall -Wextra -Werror \
  -I"$JAVA_ROOT/include" -I"$JAVA_ROOT/include/darwin" \
  ${FLAGS[@]+"${FLAGS[@]}"} \
  -o "$OUT/libspeech_observer.so" "$HERE/agent.cc" ${OBJECTS[@]+"${OBJECTS[@]}"}
if [[ "$MODE" != observe && "$MODE" != flow ]]; then
  mkdir "$OUT/classes" "$OUT/dex"
  "$JAVA_ROOT/bin/javac" -source 11 -target 11 \
    -classpath "$SDK/platforms/android-35/android.jar" \
    -d "$OUT/classes" "$HERE"/*.java
  "$SDK/build-tools/35.0.0/d8" --min-api 30 \
    --lib "$SDK/platforms/android-35/android.jar" --output "$OUT/dex" \
    "$OUT"/classes/dev/frontcap/speech/*.class
  (cd "$OUT/dex" && zip -q ../speech-worker.jar classes.dex)
fi
