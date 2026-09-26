#!/bin/sh
# dist/renderers/lsfg with the shim as libMoltenVK.dylib, the real-driver symlink, LICENSE, THIRD_PARTY.txt and source.txt
# does not build: the caller builds (with the same LSFGM_VERSION, so the compiled-in version matches source.txt)
set -e
cd "$(dirname "$0")/.."
out=dist/renderers/lsfg
# without LSFGM_VERSION, git describe rewritten to build.rs's <tag>.r<commits>.g<sha>[-dirty]
version="${LSFGM_VERSION:-$(git describe --tags --always --dirty 2>/dev/null | sed -E 's/-([0-9]+)-g([0-9a-f]+)(-dirty)?$/.r\1.g\2\3/' | grep . || echo 0.9.0-beta.1)}"
# the arm64 build packages the same way; the caller picks which one is already built
target="${LSFGM_TARGET:-x86_64-apple-darwin}"
rm -rf "$out"
mkdir -p "$out"
cp "target/$target/release/liblsfg_metal.dylib" "$out/libMoltenVK.dylib"
ln -s ../../frameworks/libMoltenVK.dylib "$out/libMoltenVK.real.dylib"
cp LICENSE "$out/LICENSE"
cp THIRD_PARTY.txt "$out/THIRD_PARTY.txt"
printf '%s\n' "lsfg-metal $version" "Source: https://github.com/itsOwen/lsfg-metal" "License: MIT (see LICENSE)" > "$out/source.txt"

codesign -s - -f "$out/libMoltenVK.dylib"

# exactly the shim's own functions plus the forwarded list; objc2 statics and the marker are data, not T
own="vkGetInstanceProcAddr vkGetDeviceProcAddr vkCreateMetalSurfaceEXT vkCreateMacOSSurfaceMVK
vkCreateInstance vkDestroyInstance vkDestroyDevice vkEnumerateInstanceExtensionProperties
vkEnumerateInstanceVersion vkEnumerateDeviceExtensionProperties"
forwarded=$(sed -n '/^forward! {$/,/^}$/p' src/shim/forward.rs | sed -nE 's/^[[:space:]]*((mvk|vk)[A-Za-z0-9_]+)[[:space:]]*$/\1/p')
expected=$(printf '%s\n' $own $forwarded | sort)
actual=$(nm -gU "$out/libMoltenVK.dylib" | awk '$2=="T" {sub(/^_/, "", $3); print $3}' | sort)
if [ "$expected" != "$actual" ]; then
  echo "export list differs from the expected one (< expected only, > exported only):" >&2
  tmp=$(mktemp -d)
  printf '%s\n' "$expected" > "$tmp/expected"
  printf '%s\n' "$actual" > "$tmp/actual"
  diff "$tmp/expected" "$tmp/actual" | grep '^[<>]' >&2 || true
  rm -rf "$tmp"
  exit 1
fi
nm -gU "$out/libMoltenVK.dylib" | grep -q " _LSFGM_SHIM$" || { echo "missing LSFGM_SHIM marker" >&2; exit 1; }

echo "packaged $out ($version, $target)"
