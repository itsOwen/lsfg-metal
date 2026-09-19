#!/bin/sh
# dist/renderers/lsfg with the shim as libMoltenVK.dylib, the real-driver symlink, LICENSE and source.txt
# does not build: the caller builds (with the same LSFGM_VERSION, so the compiled-in version matches source.txt)
set -e
cd "$(dirname "$0")/.."
out=dist/renderers/lsfg
version="${LSFGM_VERSION:-$(git describe --tags --always --dirty 2>/dev/null || echo 0.5.0)}"
rm -rf "$out"
mkdir -p "$out"
cp target/x86_64-apple-darwin/release/liblsfg_metal.dylib "$out/libMoltenVK.dylib"
ln -s ../../frameworks/libMoltenVK.dylib "$out/libMoltenVK.real.dylib"
cp LICENSE "$out/LICENSE"
printf '%s\n' "lsfg-metal $version" "Source: https://github.com/itsOwen/lsfg-metal" "License: MIT (see LICENSE)" > "$out/source.txt"

codesign -s - -f "$out/libMoltenVK.dylib"

# the names wine dlsyms and the globals a native app resolves by symbol (objc2 class statics are data, not T)
symbols=$(nm -gU "$out/libMoltenVK.dylib" | awk '$2=="T"')
count=$(printf '%s\n' "$symbols" | grep -c . || true)
[ "$count" -eq 8 ] || { echo "expected 8 exported functions, found $count:" >&2; printf '%s\n' "$symbols" >&2; exit 1; }
for symbol in vkGetInstanceProcAddr vkGetDeviceProcAddr vkCreateMetalSurfaceEXT vkCreateMacOSSurfaceMVK \
  vkCreateInstance vkEnumerateInstanceExtensionProperties vkEnumerateInstanceVersion \
  vkEnumerateDeviceExtensionProperties; do
  printf '%s\n' "$symbols" | grep -q " _$symbol\$" || { echo "missing export: $symbol" >&2; exit 1; }
done

echo "packaged $out ($version)"
