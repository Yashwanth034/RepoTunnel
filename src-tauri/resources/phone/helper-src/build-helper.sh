#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP="$ROOT/app"
OUT="${1:-$ROOT/build/repotunnel-phone-helper.apk}"
SDK="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"

fail() {
  printf 'RepoTunnel Phone helper build: %s\n' "$*" >&2
  exit 1
}

[[ -n "$SDK" && -d "$SDK" ]] || fail "ANDROID_SDK_ROOT or ANDROID_HOME must point to an Android SDK."

PLATFORM="$(find "$SDK/platforms" -mindepth 1 -maxdepth 1 -type d -name 'android-*' -print | sort -V | tail -n1)"
TOOLS="$(find "$SDK/build-tools" -mindepth 1 -maxdepth 1 -type d -print | sort -V | tail -n1)"
[[ -n "$PLATFORM" && -f "$PLATFORM/android.jar" ]] || fail "No Android platform android.jar was found."
[[ -n "$TOOLS" ]] || fail "No Android build-tools directory was found."

AAPT2="$TOOLS/aapt2"
D8="$TOOLS/d8"
APKSIGNER="$TOOLS/apksigner"
ZIPALIGN="$TOOLS/zipalign"
for tool in "$AAPT2" "$D8" "$APKSIGNER" "$ZIPALIGN"; do
  [[ -x "$tool" ]] || fail "Missing Android build tool: $tool"
done

command -v javac >/dev/null 2>&1 || fail "javac is required."
command -v keytool >/dev/null 2>&1 || fail "keytool is required."
command -v zip >/dev/null 2>&1 || fail "zip is required."

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT
mkdir -p "$TMP/classes" "$TMP/dex" "$(dirname "$OUT")"

"$AAPT2" compile --dir "$APP/src/main/res" -o "$TMP/resources.zip"
"$AAPT2" link   -o "$TMP/resources.apk"   -I "$PLATFORM/android.jar"   --manifest "$APP/src/main/AndroidManifest.xml"   --min-sdk-version 26   --target-sdk-version 35   --version-code 1   --version-name 1.0   "$TMP/resources.zip"

mapfile -t JAVA_SOURCES < <(find "$APP/src/main/java" -type f -name '*.java' -print | sort)
(("${#JAVA_SOURCES[@]}" > 0)) || fail "No Java sources were found."

javac   -encoding UTF-8   -source 8   -target 8   -classpath "$PLATFORM/android.jar"   -d "$TMP/classes"   "${JAVA_SOURCES[@]}"

mapfile -t CLASS_FILES < <(find "$TMP/classes" -type f -name '*.class' -print | sort)
(("${#CLASS_FILES[@]}" > 0)) || fail "No Java classes were produced."

"$D8"   --lib "$PLATFORM/android.jar"   --min-api 26   --output "$TMP/dex"   "${CLASS_FILES[@]}"

cp "$TMP/resources.apk" "$TMP/unsigned.apk"
zip -q -j "$TMP/unsigned.apk" "$TMP/dex/classes.dex"
"$ZIPALIGN" -f 4 "$TMP/unsigned.apk" "$TMP/aligned.apk"

KEYSTORE="${REPOTUNNEL_PHONE_HELPER_KEYSTORE:-}"
ALIAS="${REPOTUNNEL_PHONE_HELPER_KEY_ALIAS:-repotunnel-phone-helper}"
STOREPASS="${REPOTUNNEL_PHONE_HELPER_STOREPASS:-}"
KEYPASS="${REPOTUNNEL_PHONE_HELPER_KEYPASS:-}"
STOREPASS_FILE="${REPOTUNNEL_PHONE_HELPER_STOREPASS_FILE:-}"
KEYPASS_FILE="${REPOTUNNEL_PHONE_HELPER_KEYPASS_FILE:-}"

KS_PASS_ARG=""
KEY_PASS_ARG=""

if [[ -n "$STOREPASS_FILE" ]]; then
  [[ -f "$STOREPASS_FILE" ]] || fail "Configured helper store-password file does not exist."
  KS_PASS_ARG="file:$STOREPASS_FILE"
fi
if [[ -n "$KEYPASS_FILE" ]]; then
  [[ -f "$KEYPASS_FILE" ]] || fail "Configured helper key-password file does not exist."
  if [[ -n "$STOREPASS_FILE" && "$KEYPASS_FILE" == "$STOREPASS_FILE" ]]; then
    KEYPASS_COPY="$TMP/keypass"
    cp "$KEYPASS_FILE" "$KEYPASS_COPY"
    chmod 600 "$KEYPASS_COPY"
    KEY_PASS_ARG="file:$KEYPASS_COPY"
  else
    KEY_PASS_ARG="file:$KEYPASS_FILE"
  fi
fi

if [[ -z "$KEYSTORE" ]]; then
  KEYSTORE="$TMP/dev-helper.jks"
  STOREPASS="repotunnel-dev-only"
  KEYPASS="$STOREPASS"
  keytool -genkeypair     -keystore "$KEYSTORE"     -storepass "$STOREPASS"     -keypass "$KEYPASS"     -alias "$ALIAS"     -keyalg RSA     -keysize 3072     -validity 3650     -dname "CN=RepoTunnel Phone Helper Development,O=RepoTunnel"     >/dev/null 2>&1
  KS_PASS_ARG="pass:$STOREPASS"
  KEY_PASS_ARG="pass:$KEYPASS"
else
  [[ -f "$KEYSTORE" ]] || fail "Configured helper keystore does not exist."
  if [[ -z "$KS_PASS_ARG" ]]; then
    [[ -n "$STOREPASS" ]] || fail "REPOTUNNEL_PHONE_HELPER_STOREPASS or REPOTUNNEL_PHONE_HELPER_STOREPASS_FILE is required for a configured keystore."
    KS_PASS_ARG="pass:$STOREPASS"
  fi
  if [[ -z "$KEY_PASS_ARG" ]]; then
    if [[ -n "$KEYPASS" ]]; then
      KEY_PASS_ARG="pass:$KEYPASS"
    else
      KEY_PASS_ARG="$KS_PASS_ARG"
    fi
  fi
fi

"$APKSIGNER" sign   --ks "$KEYSTORE"   --ks-key-alias "$ALIAS"   --ks-pass "$KS_PASS_ARG"   --key-pass "$KEY_PASS_ARG"   --out "$OUT"   "$TMP/aligned.apk"

"$APKSIGNER" verify --verbose "$OUT" >/dev/null
printf 'RepoTunnel Phone helper APK ready: %s\n' "$OUT"
