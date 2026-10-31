#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Try the viewer on this machine: in a browser, as the Mac application, on
# the iOS simulator, or on an Android emulator or phone. One command each.
#
#   scripts/try-viewer.sh web                 # http://localhost:8091/
#   scripts/try-viewer.sh web --open          # … and open it in Chrome
#   scripts/try-viewer.sh web --dev           # unoptimised wasm: builds faster
#   scripts/try-viewer.sh web --lan           # for a phone on the same Wi-Fi
#   scripts/try-viewer.sh web --lan --https   # … with WebGPU, location and the
#                                             #     clipboard there (see below)
#   scripts/try-viewer.sh mac [flags]         # the Mac application; flags as
#                                             #     `tuile-wgpu-viewer --help`
#   scripts/try-viewer.sh ios [flags]         # the iOS simulator; flags as the
#                                             #     Mac application's
#   scripts/try-viewer.sh android             # the Android emulator, or the
#                                             #     phone on the cable
#   scripts/try-viewer.sh android --gl        # … forcing OpenGL ES
#
# A sunset, half an hour before it at the Pic du Midi, an hour of the scene every two minutes:
#   scripts/try-viewer.sh mac --lon 0.1418 --lat 42.9365 --altitude 3100 \
#     --heading 290 --pitch 0 --time sunset --time-offset -30m --time-rate 30
#   open "tuile://goto?lon=0.1418&lat=42.9365&altitude=3100&heading=290&pitch=0&time=sunset&timeOffset=-30m&timeRate=30"
# and on the web, the same parameters in the page's query (printed below).
#
# The token is read from the workspace's `.env` (the main checkout's, from a
# worktree) — TUILE_ION_TOKEN for the web, CESIUM_ION_TOKEN (or
# TUILE_ION_TOKEN) for the Mac and the simulator. It is never printed and never
# put in a URL. In the browser it never reaches the page at all: the page asks
# the catalog proxy this script starts, and the proxy holds the token. So the
# LAN mode needs no token on the phone either. Android is the same: the APK
# holds no token, and asks the proxy this script starts on this machine,
# reached from the device through `adb reverse`.
#
# A phone over plain HTTP is not a secure page: Safari and Chrome then give it
# WebGL2 but not WebGPU, nor the location or the clipboard. `--https` serves
# HTTPS with a certificate from `mkcert` (brew install mkcert), made for this
# machine's address; the phone has to trust mkcert's root once:
#   1. `mkcert -CAROOT` names a folder; AirDrop its rootCA.pem to the phone;
#   2. Settings › General › VPN & Device Management: install the profile;
#   3. Settings › General › About › Certificate Trust Settings: turn on full
#      trust for the mkcert root.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
what="${1:-}"
[[ $# -gt 0 ]] && shift

# The workspace's `.env`, or the main checkout's from a worktree.
env_file() {
  local common
  common="$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)"
  for file in "${TUILE_ENV_FILE:-}" "$root/.env" "$common/../.env"; do
    if [[ -n "$file" && -f "$file" ]]; then
      echo "$file"
      return
    fi
  done
}

# The value of one variable in the `.env`, without running the file.
env_value() {
  local file="$1" name="$2"
  sed -n "s/^[[:space:]]*\(export[[:space:]]\{1,\}\)\{0,1\}$name=//p" "$file" \
    | tail -n 1 | sed "s/^[\"']//; s/[\"']\$//"
}

# This machine's address on the local network.
lan_address() {
  local interface
  interface="$(route -n get default 2>/dev/null | awk '/interface:/ { print $2 }')"
  ipconfig getifaddr "${interface:-en0}" 2>/dev/null || ipconfig getifaddr en0
}

web() {
  local profile=release open=0 lan=0 https=0 port=8091
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --dev) profile=dev ;;
      --open) open=1 ;;
      --lan) lan=1 ;;
      --https) https=1 ;;
      --port) port="$2"; shift ;;
      *) echo "web: unknown flag $1" >&2; exit 2 ;;
    esac
    shift
  done
  local env
  env="$(env_file)"
  if [[ -z "$env" ]]; then
    echo "no .env with TUILE_ION_TOKEN: the catalog proxy needs the token" >&2
    exit 1
  fi
  if [[ "$profile" == dev ]]; then
    "$root/packages/tuile-viewer/build.sh" --dev
  else
    "$root/packages/tuile-viewer/build.sh"
  fi
  (cd "$root" && CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" cargo build --release -p tuile-catalog-proxy)

  local listen="127.0.0.1:$port" host="localhost" scheme=http tls=()
  if [[ $lan == 1 ]]; then
    host="$(lan_address)"
    listen="0.0.0.0:$port"
  fi
  if [[ $https == 1 ]]; then
    if ! command -v mkcert >/dev/null; then
      echo "--https needs mkcert: brew install mkcert && mkcert -install" >&2
      exit 1
    fi
    local certs="$root/target/try-viewer-certs"
    mkdir -p "$certs"
    (cd "$certs" && mkcert -cert-file cert.pem -key-file key.pem "$host" localhost 127.0.0.1 >/dev/null)
    tls=(--tls-cert "$certs/cert.pem" --tls-key "$certs/key.pem")
    scheme=https
  fi
  local url="$scheme://$host:$port/examples/web-viewer/"
  echo
  echo "  $url"
  echo "  $url?backend=webgl2    (forces WebGL2)"
  echo "  $url?compression=auto  (imagery block-compressed; &backend=webgl2: encoded on the CPU)"
  echo "  $url?lon=0.1418&lat=42.9365&altitude=3100&heading=290&pitch=0&time=sunset&timeOffset=-30m&timeRate=30"
  echo "                         (half an hour before sunset at the Pic du Midi, as the tuile://goto link)"
  if [[ $lan == 1 ]] && command -v qrencode >/dev/null; then
    qrencode -t ANSIUTF8 "$url"
  elif [[ $lan == 1 ]]; then
    echo "  (brew install qrencode prints a QR code to scan here)"
  fi
  echo
  if [[ $open == 1 ]]; then
    (sleep 2 && open -a "Google Chrome" "$url") &
  fi
  exec "$root/target/release/tuile-catalog-proxy" \
    --env-file "$env" \
    --assets 1,2,3,3954,2275207 \
    --listen "$listen" \
    --serve /examples/web-viewer="$root/examples/web-viewer" \
    --serve /packages/tuile-viewer="$root/packages/tuile-viewer" \
    ${tls[@]+"${tls[@]}"}
}

# Runs `$@` with the native viewer's token in its environment, read from the
# `.env` and never echoed.
with_token() {
  local env token=""
  env="$(env_file)"
  if [[ -n "$env" ]]; then
    token="$(env_value "$env" CESIUM_ION_TOKEN)"
    [[ -z "$token" ]] && token="$(env_value "$env" TUILE_ION_TOKEN)"
  fi
  if [[ -z "$token" && -z "${CESIUM_ION_TOKEN:-}" ]]; then
    echo "no CESIUM_ION_TOKEN in the environment or in .env" >&2
    exit 1
  fi
  CESIUM_ION_TOKEN="${CESIUM_ION_TOKEN:-$token}" "$@"
}

mac() {
  (cd "$root" && CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" cargo build --release -p tuile-wgpu-viewer)
  RUST_LOG="${RUST_LOG:-warn,tuile_viewer=info}" with_token "$root/target/release/tuile-wgpu-viewer" "$@"
}

ios() {
  local booted
  booted="$(xcrun simctl list devices booted | awk -F'[()]' '/Booted/ { print $2; exit }')"
  if [[ -z "$booted" ]]; then
    xcrun simctl boot "${TUILE_SIMULATOR:-iPhone 16 Pro}"
  fi
  # The simulator's window, where this Xcode ships one; without it the
  # simulator runs all the same — `xcrun simctl io booted screenshot` sees it.
  open -a Simulator 2>/dev/null || true
  # Built without a token in the bundle; the simulator is handed one in the
  # launch's environment instead, which xcodebuild never sees.
  local app
  app="$(TUILE_TILES_TOKEN=none "$root/crates/tuile-viewer/ios/build.sh" simulator | tail -n 1)"
  xcrun simctl install booted "$app"
  local token
  token="$(with_token printenv CESIUM_ION_TOKEN)"
  # Any further arguments are the viewer's own: `ios --imagery-compression auto`.
  SIMCTL_CHILD_CESIUM_ION_TOKEN="$token" xcrun simctl launch booted dev.lapoule.tuile.viewer "$@"
}

# The Android emulator (the AVD `TUILE_AVD`, by default `tuile-arm64`, booted
# if no device is attached) or the phone on the cable: the APK built and
# installed, the catalog proxy started here and mapped onto the device's own
# loopback, the application started, and its log followed until Ctrl-C.
android() {
  local gl=0 port=8091
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --gl) gl=1 ;;
      *) echo "android: unknown flag $1" >&2; exit 2 ;;
    esac
    shift
  done
  local sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
  local adb="$sdk/platform-tools/adb"
  local env
  env="$(env_file)"
  if [[ -z "$env" ]]; then
    echo "no .env with TUILE_ION_TOKEN: the catalog proxy needs the token" >&2
    exit 1
  fi
  if ! "$adb" get-state >/dev/null 2>&1; then
    local avd="${TUILE_AVD:-tuile-arm64}"
    echo "booting the emulator $avd"
    "$sdk/emulator/emulator" -avd "$avd" -gpu host -no-snapshot-save -no-boot-anim \
      >/dev/null 2>&1 &
    "$adb" wait-for-device
    until [[ "$("$adb" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == 1 ]]; do
      sleep 1
    done
  fi
  "$root/crates/tuile-viewer/android/build.sh" --install
  (cd "$root" && CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" cargo build --release -p tuile-catalog-proxy)
  "$adb" reverse "tcp:$port" "tcp:$port" >/dev/null
  "$root/target/release/tuile-catalog-proxy" \
    --env-file "$env" --assets 1,2,3,3954 --listen "127.0.0.1:$port" &
  local proxy=$!
  trap 'kill $proxy 2>/dev/null' EXIT
  local extras=(-e RUST_LOG "${RUST_LOG:-warn,tuile_viewer=info}")
  [[ $gl == 1 ]] && extras+=(-e TUILE_BACKEND gl)
  "$adb" logcat -c
  "$adb" shell am start -S -n dev.lapoule.tuile.viewer/dev.lapoule.tuile.TuileActivity "${extras[@]}" >/dev/null
  echo "following the log (Ctrl-C ends it, and the proxy)"
  "$adb" logcat -v brief -s tuile:V
}

case "$what" in
  web) web "$@" ;;
  mac) mac "$@" ;;
  ios) ios "$@" ;;
  android) android "$@" ;;
  *)
    sed -n '5,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
