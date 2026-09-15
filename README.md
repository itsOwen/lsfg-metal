# lsfg-metal (BETA)

Lossless Scaling frame generation for Wine games on macOS: a MoltenVK shim plus a Metal front end,
independent Rust implementation, MIT.

**This is a very early release.** It does what it claims: 2x, 3x and 4x hold their exact cadence
with no dropped or duplicated frames, on the Vulkan path and the Metal path, verified in real games
through Steam. It has also only ever run on one Apple Silicon Mac, a handful of titles and a single
60 Hz display. Everything outside that is untested, so expect rough edges on hardware, games and
refresh rates it has never seen. A bug report with a log attached is worth more than a star.

There is plenty left to do: wider game coverage, high-refresh and multi-display pacing, HDR10, and
a proper look at why 4x stops scaling once presentation is uncapped.

P.S. It is MIT on purpose. Fork it, vendor it, ship it inside your own launcher, or lift whichever
parts are useful and throw away the rest. Keep the licence notice and otherwise do whatever you
like with it. If you build something good on top of this, I would genuinely like to see it.

## Status

Two hooks cover the renderers a Wine bottle can use:

| Renderer | Hook |
|---|---|
| DXVK | Vulkan hook (the shim stands in for `libMoltenVK.dylib`) |
| vkd3d-proton | Vulkan hook |
| WineD3D on its Vulkan renderer | Vulkan hook |
| DXMT | Metal hook (`CAMetalLayer` presentation) |
| D3DMetal | Metal hook |

* Multipliers 2 to 4. Multiplier 1 is accepted in a config file and disables generation; above 4 is
  rejected by the settings library.
* Pacing: fixed (`vsync`, evenly spaced timestamps `1/m .. m/m`) and adaptive (the pacer fits
  generated frames to display slots).
* Performance mode (the performance shader set) and flow scale 0.25 to 1.0.
* HDR is linear `RGBA16F` only: Vulkan `R16G16B16A16_SFLOAT` with
  `VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT`, and `CAMetalLayer`s in `RGBA16Float` with
  `kCGColorSpaceExtendedLinearSRGB`. Plus single-layer RGBA8/BGRA8 sRGB swapchains.

Everything else keeps native presentation.

## How it works

Wine on macOS has no Vulkan loader. `win32u.so` `dlopen`s `libMoltenVK.dylib` directly and `dlsym`s
`vkGetInstanceProcAddr` and `vkGetDeviceProcAddr`; `winemac.so` also `dlsym`s
`vkCreateMetalSurfaceEXT` and `vkCreateMacOSSurfaceMVK`. So the shim is installed under the leaf
name `libMoltenVK.dylib` and is found by putting its directory first on `DYLD_LIBRARY_PATH`.

`DYLD_FALLBACK_LIBRARY_PATH` is not enough: `win32u.so` loads `@rpath/libMoltenVK.dylib` and
carries an `LC_RPATH` naming the directory that holds your Wine build's own `libMoltenVK.dylib`.
dyld expands `@rpath` against those rpaths before it consults the fallback path, so the real driver
would win.

The real driver is loaded under a *different* leaf name, `libMoltenVK.real.dylib` (a symlink beside
the shim). `DYLD_LIBRARY_PATH` overrides libraries by leaf name even for absolute `dlopen` paths, so
opening anything named `libMoltenVK.dylib` returns the shim itself. The shim detects that case
(the resolved `vkGetInstanceProcAddr` is its own export), logs
`resolved to this shim, not to MoltenVK (leaf name hijacked by DYLD_LIBRARY_PATH?); refusing to
recurse`, and stays passive rather than recursing. `LSFGM_MOLTENVK` overrides the path.

The Metal front end is the same dylib, injected with `DYLD_INSERT_LIBRARIES` and enabled with
`LSFGM_METAL=1`. It swizzles `-[CAMetalLayer nextDrawable]` so the game receives private drawables
owned by the shim, and swizzles the driver's command buffer class for `presentDrawable:`,
`presentDrawable:afterMinimumDuration:`, `presentDrawable:atTime:` and `commit`. `commit` hands the
frame to a per-layer worker thread, which runs the pipeline on a private Vulkan device and presents
the layer's real drawables.

With no matching profile, both hooks stay out of the way: the shim forwards every call to the real
driver and the `nextDrawable` hook returns the original drawable. It is a transparent passthrough.

## Requirements

* x86_64 Wine running under Rosetta. The shim, your Wine build's MoltenVK and the game are all x86_64.
* A MoltenVK that exposes `VK_EXT_metal_objects`. The Metal front end and the proxy swapchain
  import Metal textures and shared events through it.
* macOS 11 or newer (the deployment target of this crate).
* Your own `lsfg-vk.dll` from the *Lossless Scaling* Steam application, which you can buy at
  <https://store.steampowered.com/app/993090/>. Select the **`lsfg-vk` beta branch** under
  Properties, Betas: the default branch ships only `Lossless.dll`, whose shaders are DirectX
  bytecode rather than SPIR-V and cannot be used here. This project does not distribute the file,
  and none of this works without it, so please buy Lossless Scaling and support its developers.

## Install

`Scripts/package.sh` produces the component layout:

```
dist/renderers/lsfg/
  libMoltenVK.dylib          the shim
  libMoltenVK.real.dylib ->  ../../frameworks/libMoltenVK.dylib
  LICENSE
  source.txt                 version, license, source URL
```

Manual setup, without a launcher:

```sh
mkdir -p ~/lsfg && cp dist/renderers/lsfg/libMoltenVK.dylib ~/lsfg/
ln -sf /path/to/real/libMoltenVK.dylib ~/lsfg/libMoltenVK.real.dylib

# Vulkan path (DXVK, vkd3d-proton, WineD3D on Vulkan)
export DYLD_LIBRARY_PATH="$HOME/lsfg:$DYLD_LIBRARY_PATH"

# Metal path (DXMT, D3DMetal)
export DYLD_INSERT_LIBRARIES="$HOME/lsfg/libMoltenVK.dylib"
export LSFGM_METAL=1

# configuration
export LSFGM_ENV=1 LSFGM_MULTIPLIER=2
export LSFGM_DLL_PATH="/path/to/Lossless Scaling/lsfg-vk.dll"
```

WineD3D's OpenGL renderer presents through a path neither hook covers. Switch it to Vulkan with
`WINE_D3D_CONFIG=renderer=vulkan` when generation is on.

Instead of the symlink you can point `LSFGM_MOLTENVK` at the real driver, under any name other
than `libMoltenVK.dylib`.

## Configuration, environment mode

Setting `LSFGM_ENV` (any value, including empty) skips the config file entirely: the settings
library builds one profile named `(environment)` from the variables below and uses it.

| Variable | Meaning | Values | Default |
|---|---|---|---|
| `LSFGM_ENV` | selects environment-profile mode | read by **presence**, any value including empty | unset (file mode) |
| `LSFGM_MULTIPLIER` | frames shown per source frame | unsigned decimal, whole string, at most 2^32-1; 2 to 4 | 2 |
| `LSFGM_FLOW_SCALE` | flow resolution scale | float, 0.25 to 1.0 | 1.0 |
| `LSFGM_PERFORMANCE_MODE` | use the performance shader set | `1` is true, any other non-empty value is false | false |
| `LSFGM_PACING_MODE` | pacing | `vsync` or `none` (fixed), `adaptive`; matched case-insensitively | `vsync` |
| `LSFGM_OVERRIDE_PRESENT_MODE` | force FIFO on wrapped swapchains | `1` is true, other non-empty is false | true |
| `LSFGM_PRESERVE_SWAPCHAIN_IMAGE_COUNT` | keep the game's image count | `1` is true, other non-empty is false | false |
| `LSFGM_DLL_PATH` | path to `lsfg-vk.dll` | path | unset, see discovery below |
| `LSFGM_NO_FP16` | disable half precision | `allow_fp16 = (value != "1")` | unset, fp16 allowed |
| `LSFGM_LOG_LEVEL` | log level | `debug`, `info`, `warning`, `error` | `info` |
| `LSFGM_LOG_FILE` | append logs to this file as well as stderr | path | unset |
| `LSFGM_MOLTENVK` | real driver path | path to the real MoltenVK, any leaf name but `libMoltenVK.dylib` | `libMoltenVK.real.dylib` beside the shim |
| `LSFGM_METAL` | enable the Metal front end | enabled when set, non-empty and not `0` | unset |
| `LSFGM_VULKAN_PROXY` | proxy swapchain for adaptive pacing on the Vulkan path | `0` disables it and forces the fixed present path | unset, proxy used when supported |
| `LSFGM_TARGET_FPS` | override the display refresh used by the pacer | finite positive float, whole string | unset, the main screen's rate |
| `LSFGM_STATS` | periodic statistics lines | read by **presence** | unset |
| `LSFGM_PACE_DEBUG` | per-frame estimator log line | read by **presence** | unset |
| `LSFGM_METAL_DUMP` | frame dump directory (Metal path) | directory path | unset |
| `LSFGM_CONFIG` | configuration file path, used verbatim and highest precedence | path; must exist or loading fails | unset |
| `LSFGM_PROFILE` | select a profile by exact name | profile name | unset |
| `LSFGM_VERSION` | **build-time** override of the compiled-in version string | any string | a git-derived string, else `0.1.0` |

Read by presence alone (an empty value still counts): `LSFGM_ENV`, `LSFGM_STATS` and
`LSFGM_PACE_DEBUG`. Every other variable is read by value and is only honoured when non-empty.

Also consulted: `XDG_CONFIG_HOME` and `HOME` for the config path; `SteamAppId` for profile
selection; `HOME` and `WINEPREFIX` for DLL discovery; `HOME` and `XDG_CACHE_HOME` for the pipeline
cache directory.

There is no `LSFGM_HDR`. HDR is decided by the swapchain or layer format and color space, not by a
variable.

Validation errors in environment mode: `Invalid LSFGM_MULTIPLIER`,
`The macOS shim supports multipliers from 2 to 4`, `LSFGM_MULTIPLIER must be greater than 1`,
`LSFGM_FLOW_SCALE must be between 0.25 and 1.0`.

When `LSFGM_DLL_PATH` is unset the shader package is looked for in Steam under `HOME`, then in
Steam inside the Wine prefix named by `WINEPREFIX`, then in the working directory. A launcher
that already knows where the file is should set the variable instead of relying on the search.

## Configuration, file mode

Without `LSFGM_ENV` the configuration comes from a TOML file. The path is chosen in this order,
**last match wins**:

1. `/etc/lsfg-metal/conf.toml`
2. `$HOME/.config/lsfg-metal/conf.toml`, if `HOME` is set and non-empty
3. `$XDG_CONFIG_HOME/lsfg-metal/conf.toml`, if set and non-empty
4. `$LSFGM_CONFIG`, if set and non-empty, used verbatim

If the chosen file does not exist and `LSFGM_CONFIG` was set, loading fails with
`LSFGM_CONFIG is set but file does not exist: <path>`. Otherwise the built-in default file is
written to that path (creating parent directories) and used.

```toml
version = 2

[global]
dll = "~/Games/Lossless Scaling/lsfg-vk.dll"
allow_fp16 = true
log_level = "info"
# log_file = "~/lsfg.log"

[[profile]]
name = "Default 2x"
active_in = "000000"                 # Steam App IDs: a string, or an array of strings
pacing_mode = "vsync"                # "pacing" is accepted as an alias
multiplier = 2
flow_scale = 1.0
performance_mode = false
override_present_mode = true
preserve_swapchain_image_count = false
```

Supported subset of TOML:

* Single-line values only: basic strings (`"..."` with `\n \t \r \" \\` escapes), literal strings
  (`'...'`), booleans, integers, floats, and one-line arrays. No multi-line strings, no inline
  tables, no dotted keys.
* `~` at the start of `dll` and `log_file` expands to `$HOME` (only when followed by `/` or by
  nothing).
* In a profile, the key `pacing` is accepted as an alias for `pacing_mode`.
* `#` starts a comment; a leading UTF-8 BOM is tolerated.
* `version` must be the integer `2`; `[global]` must be present; `[[profile]]` is an array of
  tables. Unknown keys are errors (`Unknown key in configuration: <key>`,
  `Unknown key in [global] section: <key>`, `Unknown key in profile section: <key>`).
* A profile's `multiplier` must be 1 to 4 (above 4 is `Profile multipliers must be 1 to 4`, below 1
  is `Profile '<name>' has multiplier < 1`) and its `flow_scale` 0.25 to 1.0. Multiplier 1 is
  accepted here and disables generation; environment mode is stricter and requires 2 to 4.

Reload on change: the shim stats the config file at every present and records its modification time
as `(seconds, nanoseconds)`. When the mtime changes it reparses, logs
`Config file changed on disk, reloading...`, and adopts the profile with the **same name** as the
active one. If that name is gone the old profile is kept. A parse failure leaves the recorded mtime
untouched, so the next present retries the same file; this is what covers a half-written file. The
watcher is not created when `LSFGM_ENV` is set. Only the Vulkan fixed present path checks the
watcher: the Metal front end and the Vulkan proxy path capture the profile once when they are set
up, so a config change does not reach them until the swapchain or layer is rebuilt.

Profile selection order:

1. `LSFGM_ENV` set: the synthesised `(environment)` profile, method *environment*.
2. `LSFGM_PROFILE` non-empty: the profile whose `name` equals it exactly, method *environment*.
3. `SteamAppId` non-empty: the first profile whose `active_in` contains exactly that string,
   method *Steam App ID*.
4. No match: no profile, and the shim stays passive.

Matching is by `SteamAppId` only; executable names are not inspected.

## Adaptive pacing

With `pacing_mode = "adaptive"` the multiplier becomes a *cap* and the pacer decides how many frames
to show per source frame.

A **sample** per source frame is `{interval, trusted}`. `interval` is the time since the previous
present. `trusted` is `blocked < max(0.001 s, interval * 0.1)`, where `blocked` is the time the
game's thread spent waiting inside the shim's hooks (for a drawable or a proxy image). A frame that
waited was ahead of presentation, so its interval measures the display's cadence rather than the
game's and is not trusted. The first sample is never trusted.

Slots are fitted against the display refresh interval:

* An unknown or hitching interval (non-finite, zero or negative, or longer than
  `refresh * cap * 4`) returns a single slot `[1]` and does not disturb the estimate.
* Trusted samples smooth the estimate with factor 0.2; untrusted ones feed a separate cadence mean
  and can replace the estimate after 10 consecutive untrusted frames if that cadence is faster.
* After 60 consecutive untrusted samples every frame probes with one slot until a trusted sample
  arrives, and that trusted sample replaces the estimate outright, without smoothing.
* If `estimate / refresh` is within 0.1 of a whole number, or rounds to at least the cap, the pacer locks
  to that many evenly spaced slots (`1/n .. n/n`). A game at or above the refresh rate gets `[1]`
  and is left alone.
* Otherwise the ratio is fractional and slots are placed at a running phase, so a 45 fps game on
  60 Hz repeats 1, 1, 2 for an average of about 1.33 slots per frame.

All timestamps are in `(0, 1]`, strictly increasing, and at most `cap` of them. A trailing
timestamp of 1 means the source frame itself is shown in that slot.

`LSFGM_TARGET_FPS` sets the refresh interval to `1 / value` when it parses as a finite positive
float. Otherwise the refresh comes from `NSScreen.mainScreen.maximumFramesPerSecond`, falling back
to 60. Caveat: that is the **main** screen, not the screen the game's window is on, so a game on a
secondary display with a different refresh rate will be paced against the wrong interval. Use
`LSFGM_TARGET_FPS` in that case.

On the Metal path a frame cap implemented by holding the drawable
(`presentDrawable:afterMinimumDuration:` with a duration longer than the measured interval) is taken
as a trusted sample of that duration, and the requested duration is divided by the number of frames
shown so the inserted frames fit inside it.

On the Vulkan path adaptive pacing needs the proxy swapchain, because the present hook cannot
otherwise measure the game's own frame time. Set `LSFGM_VULKAN_PROXY=0` to disable the proxy and
force the fixed present path. If the proxy cannot be created the shim falls back on its own and logs
`Vulkan proxy swapchain failed, using the fixed present path: <what>`.

## Logging and diagnostics

Every line is:

```
(lsfg-metal) [<LEVEL>]: <message>
```

written to stderr, and appended to `LSFGM_LOG_FILE` (or the config's `log_file`) when one is set.
Levels are `DEBUG`, `INFO`, `WARN`, `ERROR`; messages below the configured level are dropped. The
level before the configuration is loaded is `debug`. Generator library messages are re-emitted at
`DEBUG`.

At startup the shim logs the version, the profile and its settings:

```
(lsfg-metal) [INFO]: Loaded lsfg-metal layer version <version> pid=<pid>
(lsfg-metal) [INFO]: Using profile with name '<name>' (identified via environment)
(lsfg-metal) [INFO]:   Pacing: adaptive
(lsfg-metal) [INFO]:   Multiplier: 4
(lsfg-metal) [INFO]:   Flow scale: 1.00
(lsfg-metal) [INFO]:   Performance mode: false
```

**`LSFGM_STATS`.** On the Vulkan fixed path, every 60th original present:

```
Frame generation stats pid=<pid> original=<o> generated=<g> total=<o+g> (successful presents on this swapchain)
```

On the Metal path, every 60th source frame:

```
Frame generation stats pid=<pid> source=<s> original=<o> generated=<g> total=<o+g> source_fps=<fps> slots=<histogram> (metal presents on this layer)
```

Field by field:

* `pid`: the process id, so a multi-process game can be told apart.
* `source`: source frames handled by the presenter, whether or not they were shown.
* `original`: source frames actually displayed. Under adaptive pacing a fractional pattern can skip
  a source frame, so `original` can be lower than `source`.
* `generated`: interpolated frames presented.
* `total`: `original + generated`, the frames that reached the screen.
* `source_fps`: `round(samples / seconds)` over the window since the last stats line, then the
  window resets. Present only when measurable. This is the game's own rate, not the displayed rate.
* `slots`: the slot-count histogram, adaptive only, as `<n>:<count>` pairs for `n = 1..8` with
  non-zero counts, joined by single spaces, for example `1:40 2:20` (40 source frames got one slot,
  20 got two). The counts are cleared each time they are printed.

**`LSFGM_PACE_DEBUG`.** One INFO line per source frame on the Metal path:

```
pace interval=<ms>ms blocked=<ms>ms
```

with ` (untrusted)` appended when the sample was not trusted. This is how to see why the pacer
picked the slot counts it did.

**`LSFGM_METAL_DUMP=<dir>`.** On the Metal path, source frame 89 is written as `<dir>/previous`,
source frame 90 as `<dir>/original`, and each generated frame of frame 90 as `<dir>/generated<i>`.
Format is PPM (`P6 <w> <h> 255\n` then RGB bytes, B and R swapped for BGRA formats, alpha dropped),
or PFM for `RGBA16Float` (`PF\n<w> <h>\n-1.0\n` then bottom-to-top RGB float32 rows). With the
`mtlclear` smoke test, whose white square moves 8 px per frame, the generated frames must show the
square at intermediate positions. That is the check that the output is interpolation and not a copy.

## Building and packaging

The shim is x86_64: Wine and your Wine build's MoltenVK run under Rosetta. `.cargo/config.toml` pins the
target and the 11.0 deployment target, so a plain `cargo build` cross-compiles correctly on Apple
Silicon once the target is installed.

```sh
rustup target add x86_64-apple-darwin
cargo build --release
cargo test --release
Scripts/package.sh
```

`Scripts/package.sh` does not build. It copies the already built
`target/x86_64-apple-darwin/release/liblsfg_metal.dylib` into `dist/renderers/lsfg/` as
`libMoltenVK.dylib`, creates the `libMoltenVK.real.dylib` symlink, copies `LICENSE`, writes
`source.txt`, ad hoc signs the dylib with `codesign -s - -f`, and verifies the export list.

Version string. Two independent steps derive one, and they use different rules.

* `build.rs` sets the compiled-in version, which is what the startup log line reports. It takes
  `LSFGM_VERSION` when that is set and non-empty. Otherwise it asks git for the short HEAD sha and
  the nearest tag: exactly on a tag the version is the tag, otherwise `<tag>.r<commits>.g<sha>`,
  with `unknown` in place of the tag when the repository has none, and `-dirty` appended when
  tracked files are modified. With no git at all it falls back to `0.1.0`.
* `Scripts/package.sh` writes the version in `source.txt`. It takes `LSFGM_VERSION` when set,
  otherwise `git describe --tags --always --dirty`, otherwise `0.1.0`.

The script does not build, so the compiled-in version and `source.txt` agree only when
`LSFGM_VERSION` is exported for both the `cargo build` and the packaging step. Left to their own
git queries the two produce different strings from the same commit.

Export check. The shim carries the driver's install name (`@rpath/libMoltenVK.dylib`) and must
export exactly the four functions Wine `dlsym`s:

```sh
nm -gU dist/renderers/lsfg/libMoltenVK.dylib | awk '$2=="T"' | wc -l   # must be 4
```

The four are `vkGetInstanceProcAddr`, `vkGetDeviceProcAddr`, `vkCreateMetalSurfaceEXT` and
`vkCreateMacOSSurfaceMVK`. Every other entry point is reachable only through the two `ProcAddr`
functions. `objc2`'s class data statics are also exported, and that is expected: `rustc` owns the
export list for a `cdylib` and adding a second `-exported_symbol` flag makes the linker refuse the
link. They are data symbols, not text symbols, which is why the check filters on `T`.

## Testing

**Unit tests.** 26 tests, no GPU and no display needed: `cargo test --release`. They cover the pacer
(trust rule, locking, fractional ratios, cap behaviour, untrusted runs and probing, invalid
intervals, a closed-loop convergence model), the settings library (environment mode, config path
precedence, TOML round trip and `~` expansion, error messages, profile identification order, reload
on mtime change), the PE resource walk, the feature-chain copy, memory type selection, the memory
planner, the pipeline signature tables, the recursive mutex and half-float conversion.

**`validate`.** Runs the generator on a real driver with synthetic input and reports timings and the
centre pixel of the last generated frame.

```
usage: validate [--driver dylib] [--dll lsfg-vk.dll] [-w W] [-h H] [-m M] [-f flow]
                [-n iterations] [-p] [--no-fp16] [--hdr] [--partial]
```

Defaults are 1920x1080, `-m 2`, `-f 1.0`, `-n 10`, quality, fp16 on, SDR. `--driver` falls back to
`LSFGM_MOLTENVK` and `--dll` to `LSFGM_DLL_PATH` then automatic discovery; without either it exits
with `no driver: pass --driver or set LSFGM_MOLTENVK` or the matching DLL message. `--partial` runs
the regression for an iteration that never submits its completion fence and then returns. A normal
run, without `--partial`, is the one that asserts the sync semaphore advanced by exactly `2(m-1)`
per iteration.

**`shader-check`.** `shader-check <lsfg-vk.dll>`. No GPU and no driver needed. For every combination
of quality/performance, fp16/fp32 and SDR/HDR it checks the SPIR-V magic word and compares each
shader's image `Arrayed` operand against the view type the pipeline binds. On success it prints:

```
Shader image-view checks passed: 2048 bindings across quality, performance, fp16 and hdr
```

Any mismatch prints `<shader> binding <n> has the wrong image view type (resource <key>)` and exits
1.

**`vkclear`.** `cargo run --release --example vkclear -- <libMoltenVK.dylib> [frames]`, frames
defaults to 240. It loads the driver by the path given, clears a swapchain image with one of two
colours on alternate frames and presents FIFO. `VKTEST_FPS` paces the source like a capped game. Point it at
the shim and set `LSFGM_MOLTENVK` to the real driver:

```sh
LSFGM_MOLTENVK=/path/to/real/libMoltenVK.dylib \
LSFGM_ENV=1 LSFGM_MULTIPLIER=2 LSFGM_STATS=1 LSFGM_DLL_PATH=".../lsfg-vk.dll" \
cargo run --release --example vkclear -- dist/renderers/lsfg/libMoltenVK.dylib 240
```

A correct run prints the shim's startup lines, periodic `Frame generation stats` lines, and ends
with `vkclear: presented <n> frames`. On a 60 Hz vsynced display with multiplier `m` the application
rate settles near `60/m`, because the generated frames fill the gaps.

**`mtlclear`.** `cargo run --release --example mtlclear -- [frames]`. It clears a `CAMetalLayer`
drawable and blits a 32x32 white square that moves 8 px per frame, which is what makes interpolation
visible. `MTLTEST_FPS` paces the source rate; `MTLTEST_MINDURATION=<fps>` presents with
`afterMinimumDuration: 1/fps` (a value of 1 or less means 1/60); `MTLTEST_WIDTH` and
`MTLTEST_HEIGHT` size the window, 640x480 by default. It needs the shim injected:

```sh
DYLD_INSERT_LIBRARIES=dist/renderers/lsfg/libMoltenVK.dylib LSFGM_METAL=1 \
LSFGM_MOLTENVK=/path/to/real/libMoltenVK.dylib \
LSFGM_ENV=1 LSFGM_MULTIPLIER=2 LSFGM_STATS=1 LSFGM_DLL_PATH=".../lsfg-vk.dll" \
cargo run --release --example mtlclear -- 240
```

A correct run logs `lsfg-metal metal front end active` and a
`Metal presentation <w>x<h> (<format>), <multiplier m|adaptive up to m>, display <n> Hz` line,
streams `Frame generation stats` with `source`, `original` and `generated` all advancing, and
prints `<n> frames in <s>s = <f> fps` at the end. On 60 Hz that rate is about 30 at 2x, 20 at 3x and
15 at 4x. With `MTLTEST_FPS=45 LSFGM_PACING_MODE=adaptive LSFGM_MULTIPLIER=4` the stats line shows
`slots=1:40 2:20`.

Run the native examples with the `DYLD_*` variables set directly. Never wrap them in `arch`, `env`,
`perl` or another SIP-protected binary: those strip `DYLD_*` from the environment they pass on.

## Limitations

* HDR10 and PQ are not supported. HDR means linear `RGBA16F` in extended linear sRGB and nothing
  else; a PQ or HDR10 swapchain or layer keeps native presentation.
* Multi-GPU Intel Macs are not handled. The Metal front end's private backend takes the first
  enumerated Vulkan device, which is the layer's device on every Apple Silicon Mac but is not
  guaranteed to be on a two-GPU Intel Mac.
* The refresh rate comes from the main screen only. A game on a secondary display with a different
  refresh rate is paced against the wrong interval unless `LSFGM_TARGET_FPS` is set.
* A Wine build with a hardened runtime and no `com.apple.security.cs.allow-dyld-environment-variables`
  entitlement ignores `DYLD_INSERT_LIBRARIES`, so the Metal front end never activates. The same
  applies to any SIP-protected wrapper in the launch chain, which strips `DYLD_*` before exec.
* Multi-layer and protected swapchains keep native presentation, as do multi-swapchain presents and
  presents on a queue other than the adopted one. Presentation extension payloads are not in that
  list: the fixed path forwards the payload on the original present and still generates. Only the
  proxy swapchain is restrictive, accepting `VK_KHR_incremental_present` (`VkPresentRegionsKHR`)
  and recreating with fixed pacing for any other payload.

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

This project is MIT licensed. See `LICENSE`. The shader package it loads is not covered by that
license and remains the property of Lossless Scaling.
