# lsfg-metal (BETA)

Lossless Scaling frame generation for Wine games and native Metal games on macOS: a MoltenVK shim
plus a Metal front end, independent Rust implementation, MIT.

It multiplies a game's frame rate by 2x, 3x or 4x by generating frames in between, using the shaders
from your own copy of [Lossless Scaling](https://store.steampowered.com/app/993090/). On the Vulkan
and Metal paths it holds that exact cadence with no dropped or duplicated frames. It can also
upscale frames to the window's full size with Apple's MetalFX, with frame generation or on its own.

> **This is a very early release.** It has only ever run on one Apple silicon Mac, a handful of
> titles and a single 60 Hz display. Expect rough edges elsewhere. A bug report with a log attached
> is worth more than a star.

**Full documentation lives in the [wiki](https://github.com/itsOwen/lsfg-metal/wiki).**

## How it works

Wine on macOS loads MoltenVK directly, so the shim stands in for `libMoltenVK.dylib`, forwards
everything to the real driver, and takes over the game's swapchain. Metal and OpenGL games are
reached by injecting the same library, which hooks `CAMetalLayer` presentation and
`-[NSOpenGLContext flushBuffer]`. Each game frame is handed to a generator that runs the Lossless
Scaling shaders and presents the generated frames in between, paced to the display. On Apple
silicon the shaders are translated to Metal and generated natively; elsewhere they run on MoltenVK.
A game with no matching profile is passed through untouched.

| Your game uses | Hook |
|---|---|
| DXVK, vkd3d-proton, WineD3D on Vulkan | Vulkan |
| DXMT, D3DMetal (including Metal 4 / GPTK 4) | Metal |
| WineD3D on OpenGL, OpenGL games | OpenGL |
| Native macOS games and emulators | Metal or OpenGL, the build matching the game's architecture |

The details are in [How It Works](https://github.com/itsOwen/lsfg-metal/wiki/How-It-Works) and
[Compatibility](https://github.com/itsOwen/lsfg-metal/wiki/Compatibility).

## Requirements

* macOS 11 or newer. Only an M2 on macOS 27 has been tested.
* **Your own `lsfg-vk.dll`.** Buy [Lossless Scaling](https://store.steampowered.com/app/993090/) on
  Steam, then select the **`lsfg-vk` beta branch** under Properties, Betas. The default branch only
  has `Lossless.dll`, which cannot be used. This project does not distribute the file.

MoltenVK versions and the rest are in
[Requirements](https://github.com/itsOwen/lsfg-metal/wiki/Requirements).

## Install

**Easiest: [Highball](https://github.com/gauthierpiarrette/highball).** It ships lsfg-metal (an
earlier release, v0.7.x at the time of writing, without MetalFX upscaling). Install a game, then
turn frame generation on in the bottle's settings. See
[Install with Highball](https://github.com/itsOwen/lsfg-metal/wiki/Install-with-Highball).

Otherwise, download a [release](https://github.com/itsOwen/lsfg-metal/releases) and follow the guide
for your setup:

* Windows games: [CrossOver](https://github.com/itsOwen/lsfg-metal/wiki/Install-in-CrossOver), or
  [your own Wine setup or another launcher](https://github.com/itsOwen/lsfg-metal/wiki/Install-Manually)
* macOS games: [native games](https://github.com/itsOwen/lsfg-metal/wiki/Native-macOS-Games),
  [Steam games on macOS](https://github.com/itsOwen/lsfg-metal/wiki/Steam-Games-on-macOS), [Mac App Store games](https://github.com/itsOwen/lsfg-metal/wiki/Mac-App-Store-Games),
  [Cemu](https://github.com/itsOwen/lsfg-metal/wiki/Cemu) and [other emulators](https://github.com/itsOwen/lsfg-metal/wiki/Other-Emulators)

With your own Wine build, the short version is (run it where you unpacked the x86_64 archive,
`lsfg-v<version>.tar.xz`):

```sh
mkdir -p ~/lsfg && cp renderers/lsfg/libMoltenVK.dylib ~/lsfg/
ln -sf /path/to/real/libMoltenVK.dylib ~/lsfg/libMoltenVK.real.dylib

# Vulkan games (DXVK, vkd3d-proton, WineD3D on Vulkan)
export DYLD_LIBRARY_PATH="$HOME/lsfg:$DYLD_LIBRARY_PATH"
# or Metal games (DXMT, D3DMetal), OpenGL included
export DYLD_INSERT_LIBRARIES="$HOME/lsfg/libMoltenVK.dylib" LSFGM_METAL=1

export LSFGM_ENV=1 LSFGM_MULTIPLIER=2
export LSFGM_DLL_PATH="/path/to/Lossless Scaling/lsfg-vk.dll"
```

## Usage

The basic settings, as environment variables:

| Variable | What it does | Values |
|---|---|---|
| `LSFGM_MULTIPLIER` | frames shown per game frame | `2`, `3`, `4` (`1` with a scaler) |
| `LSFGM_SCALER` | MetalFX upscaling to the window's size | `metalfx`, off by default |
| `LSFGM_FLOW_SCALE` | lower is faster on the GPU | `0.25` to `1.0`, or `auto` |
| `LSFGM_PERFORMANCE_MODE` | lighter shaders | `1` to enable |
| `LSFGM_PACING_MODE` | `vsync`: fixed; `adaptive`: fits frames to the display | `vsync`, `adaptive` |
| `LSFGM_DISABLE` | turn it off for this launch | any value |

For per-game profiles, use a config file at `~/.config/lsfg-metal/conf.toml` instead. See
[Basic Usage](https://github.com/itsOwen/lsfg-metal/wiki/Basic-Usage), [Advanced Usage](https://github.com/itsOwen/lsfg-metal/wiki/Advanced-Usage),
[Configuration](https://github.com/itsOwen/lsfg-metal/wiki/Configuration) and [Upscaling](https://github.com/itsOwen/lsfg-metal/wiki/Upscaling).

**Is it working?** Set `LSFGM_STATS=1`, and `LSFGM_LOG_FILE=/path/to/log` if you cannot see the
terminal. The log should show `Loaded lsfg-metal layer`, `Using profile with name ...`, then
`Frame generation stats ... generated=<n>` lines. If it does not, see
[Troubleshooting](https://github.com/itsOwen/lsfg-metal/wiki/Troubleshooting).

## Building

```sh
rustup target add x86_64-apple-darwin
cargo build --release
Scripts/package.sh        # writes dist/renderers/lsfg/
```

You need a C++ compiler (`xcode-select --install`). See
[Building and Packaging](https://github.com/itsOwen/lsfg-metal/wiki/Building-and-Packaging).

P.S. It is MIT on purpose. Fork it, vendor it, ship it inside your own launcher, or lift whichever
parts are useful and throw away the rest. Keep the licence notice and otherwise do whatever you
like with it. If you build something good on top of this, I would genuinely like to see it.

## Affiliation and disclaimer

lsfg-metal is a community project. It is not affiliated with, endorsed by, sponsored by or
supported by Lossless Scaling or its developers, by Apple, by Valve, or by any other frame
generation implementation.

It contains no part of the Lossless Scaling application and distributes nothing belonging to it.
The application's own library is never opened. What this project reads is the Vulkan shader
package that Lossless Scaling publishes on the `lsfg-vk` beta branch of their Steam application,
from the copy you installed yourself, and it loads those shader modules without modifying them.
If you do not own Lossless Scaling, nothing here does anything. It is inexpensive, it is the
reason any of this is possible, and it deserves your money: buy it at
<https://store.steampowered.com/app/993090/>.

Running Windows games through a translation layer, and inserting a library into them, can violate
the terms of service of a game, a launcher or an anti-cheat system. Deciding whether a particular
use is permitted is your responsibility, not this project's. The software is provided as is, with
no warranty of any kind, and the authors accept no liability for how it is used or for anything
that results from using it. See `LICENSE` for the binding text.

## Provenance and license

This is an independent implementation. It was written from a functional specification of
observable behaviour rather than from any existing sources or projects.

This project is MIT licensed. See `LICENSE`. The library statically links SPIRV-Cross and
third-party Rust crates, including the Rust standard library; their notices are in
`THIRD_PARTY.txt`, which ships beside the library. The shader package it loads is not covered by
that license and remains the property of Lossless Scaling.
