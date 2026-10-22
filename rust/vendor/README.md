# Vendored third-party crates

These are copies of upstream crates with small build changes. Our crates use
them as **path dependencies**, so builds from a git checkout (including git
dependencies on the public repository) get the changes. A `[patch]` section
would only apply to builds rooted in our own workspace.

## webrtc-audio-processing 2.1.0 and webrtc-audio-processing-sys 2.1.0

- Upstream: <https://github.com/tonarino/webrtc-audio-processing>, commit
  `c14d7af1760baff83e8210fee336a0cae0faaa7d` (the crates.io 2.1.0 releases).
- Licence: the files `COPYING` in each crate, unchanged (WebRTC's BSD-style
  licence, plus the bundled sources' own notices).
- Local version note: `2.1.0+gridspawn.2`. The crate names and versions are
  unchanged.

Changes, all in `webrtc-audio-processing-sys/build.rs` unless noted:

1. meson `--buildtype=release` on every platform. The bundled project's
   default build type is `debugoptimized` (-O2 with debug info); `release` is
   -O3 without debug info. The WebRTC sources are always compiled with
   `-DNDEBUG`; meson's `b_ndebug` (false by default, also under `release`)
   only affects the abseil subproject. On x86_64 Linux (GCC 11), AEC3 on 10 s
   of 48 kHz mono plus noise suppression on 5 s took a median 0.160 s of user
   CPU with `debugoptimized` and 0.114 s with `release` (20 interleaved runs
   of each, clean builds, identical output): about 1.4 times less.
   `-Db_ndebug=true` changed either by about 0.01 s.
2. MSVC: meson `-Dcpp_std=c++20 -Db_vscrt=md`, and the wrapper is compiled
   with `/std:c++20` instead of `-std=c++17 -Wno-unused-parameter`. The sources
   use designated initializers, which MSVC accepts only from C++20 on (error
   C7555); `cl` rejects the GCC flags (D8021); Rust links the release CRT.
3. MSVC: no `v2_` symbol prefixing. `cc` names the wrapper
   `…wrapper.lib` there, so its references were not rewritten and linking
   failed (LNK2019). The prefix only exists so several versions can coexist in
   one binary.
4. The bundled sources are copied with `std::fs` instead of `cp -a`, so
   Windows builds do not need `cp` on `PATH`.
5. Windows: link `winmm`. `rtc::SystemTimeNanos()` calls `timeGetTime()`;
   a program that does not otherwise link winmm failed with LNK2019
   (`__imp_timeGetTime`).
6. `webrtc-audio-processing/Cargo.toml`: the dependency on
   `webrtc-audio-processing-sys` also has `path = "../webrtc-audio-processing-sys"`.

clang-cl is not an alternative on Windows: the AVX2 code of AEC3 uses
MSVC-only vector member access under `_MSC_VER`.

**Maintenance.** This copy is maintained by Gridspawn. Upstream changes are
pulled in deliberately, when they are useful, not automatically. Each time,
record here the upstream commit they came from and what changed.

## Building

The `bundled` feature builds WebRTC's audio processing module with meson:

- Linux: meson and ninja, a C++17 compiler, libclang (for bindgen).
- Windows: Visual Studio 2022 with the x64 developer environment (`vcvars64`),
  Python with meson, ninja, and LLVM (libclang, for bindgen).
- Windows: keep the build directory's path short (for example
  `--target-dir C:\t`), or enable long paths.
  - The bundled sources, abseil and the installed headers sit deep under
    Cargo's `OUT_DIR`, and paths over 260 characters fail with C1083.
  - Release builds under a long `OUT_DIR` have also failed silently inside
    meson: `meson setup` or `meson install`, with no output and no log. A
    short target directory avoided that. The cause is not established: the
    same long path also built once.
  - The build script names this when a meson or ninja step fails on Windows.
- Both: network access at build time, because meson fetches abseil
  (`abseil-cpp 20240722.0`) through its wrap. Vendoring the wrap's package
  cache is the follow-up for reproducible offline builds.
