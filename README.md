# VIctory on NVIDIA

A fork of [VIctory](https://github.com/Marvisak/VIctory), a compatibility layer
that runs an Xbox Series title natively on Windows from files **you** supply,
tuned for **NVIDIA GeForce RTX** cards.

> **This repository contains no game files, code or assets.** You need your own
> legally obtained dump of the title. It is not affiliated with or endorsed by
> the title's developer or publisher, nor by NVIDIA.
> See [docs/asset-policy.md](docs/asset-policy.md).

---

## Contents

- [Reporting a problem](#reporting-a-problem)
- [Quick start](#quick-start)
- [Requirements](#requirements)
- [Frame generation: what your card supports](#frame-generation-what-your-card-supports)
- [In the game](#in-the-game)
- [Building and running from source](#building-and-running-from-source)
- [Performance on an RTX 4070](#performance-on-an-rtx-4070)
- [Known issues](#known-issues)
- [Documentation](#documentation)
- [Credits](#credits)
- [License](#license)

---

## Reporting a problem

Please attach **both** logs from the run that went wrong. Paste `%LOCALAPPDATA%`
into File Explorer's address bar to get there.

1. **The play log:** the newest
   `%LOCALAPPDATA%\victory\logs\victory-nvidia-play-<date>-<time>-<number>.log`.
   The window prints its path at the start, on the line `Log: ...`.
2. **The crash log:** `%LOCALAPPDATA%\victory\data\7952987A\crashes.log` and,
   if there is one for that run, the newest
   `victory-<date>-<time>-<number>.crash.log` in
   `%LOCALAPPDATA%\victory\data\7952987A\temp\`.

Also say which GPU, Windows version (10 or 11) and release you used.

---

## Quick start

1. Download `victory.exe` from the
   [latest release](https://github.com/trustno1-coder/VIctory-NVIDIA/releases/latest).
2. Put it in the folder of **your own dump** of the title's Xbox Series build
   v1.0.2.1, next to `game_scarlett_master.exe`.
3. Double-click it. Windows may warn that the publisher is unknown (the file is
   not signed): click **More info**, then **Run anyway**.

What happens on the first start:

- **NVIDIA files.** It asks once whether to download NVIDIA's DLSS and frame
  generation files (~70 MB) from NVIDIA's own GitHub. They are NVIDIA's, not
  part of this program. If you say no, the game still runs, with its own
  upscaler and without frame generation.
- **Warm-up restart.** The first start of every new `victory.exe` plays about
  30 seconds and then restarts by itself, once. A new build runs at about 14 fps
  until that happens. A mark is kept in `%LOCALAPPDATA%\victory\warmed`.
- **Windows 10 only.** Its built-in Direct3D 12 stops at shader model 6.5 and the
  translated shaders need 6.6. The first start offers to fetch Microsoft's D3D12
  Agility SDK (`D3D12Core.dll`, ~22 MB, from nuget.org, signature-checked) into a
  `D3D12` folder beside `victory.exe`. `victory.exe get-d3d12` does the same by
  hand. Windows 11 uses its own runtime.

Everything the layer keeps (settings, saves, caches, logs, downloads) lives in
`%LOCALAPPDATA%\victory`. If your C: drive is full, run
`setx VICTORY_HOME D:\victory` once and start `victory.exe` from a new window.

---

## Requirements

- Windows 10 or 11 (x64), a CPU with AVX2, **32 GB of RAM or more**, an SSD.
- An NVIDIA GeForce RTX card with a recent driver. Frame generation needs an
  RTX 40 or later with hardware-accelerated GPU scheduling on; more than 2x needs
  an RTX 50.
- Your own dump of the title's Xbox Series build v1.0.2.1.

---

## Frame generation: what your card supports

| Your card | Frame generation | Limits |
| --- | --- | --- |
| GeForce RTX 40 / RTX 50 | **DLSS frame generation** | 2x on RTX 40; up to 6x on RTX 50 |
| GeForce RTX 20 / RTX 30 | **AMD FSR 3 frame generation** | 2x at most. DLSS upscaling still works |
| Cards without DLSS | **AMD FSR 3 frame generation** | 2x at most, Series S profile only. Not tested |

- At every start, the top of the picture says which one your card runs, and why
  not if it can't.
- F1 menu, **FRAME GEN TYPE**: AUTO, DLSS or FSR 3. **FRAME GENERATION** turns it
  on or off.
- FSR 3 needs AMD's FidelityFX DLLs (~30 MB). The first start asks once whether
  to download them from AMD's GitHub; `victory.exe get-fsr` does it later. Each
  file is checked against the tested copy and AMD's signature.
- RTX 20/30 owners can try DLSS frame generation through the third-party
  [MFG Unlocker](https://github.com/dashdogy/MFG-Unlocker): **F1, MFG UNLOCKER,
  ON**, then restart. It is not ours and is off by default.

---

## In the game

- **F1:** settings menu (arrow keys to pick and change, Enter to apply): DLSS
  mode, frame generation, frame-rate cap, console profile (Series X/S),
  resolution and the main speed switches. Saved to
  `%LOCALAPPDATA%\victory\victory-settings.txt`; **SAVE AND RESTART NOW** applies
  them.
- **F3:** stats panel: frame rate and frame time, frames shown with frame
  generation, draws, GPU and submitting-thread load, copies, RAM and VRAM.

---

## Building and running from source

Install [Rust](https://rustup.rs) (the toolchain is pinned) and the
[Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
with "Desktop development with C++". Then, in PowerShell:

```powershell
git clone https://github.com/trustno1-coder/VIctory-NVIDIA
cd VIctory-NVIDIA
.\Get-NVIDIA-DLSS.cmd        # once: NVIDIA's DLSS / frame generation files (~70 MB)
cargo build --release -p victory
.\target\release\victory.exe play 'D:\Games\MyDump\game_scarlett_master.exe'
```

Or use the launcher script, which builds on first use and takes options:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\play-nvidia.ps1 -Exe 'D:\Games\MyDump\game_scarlett_master.exe'
```

| Option | Effect |
| --- | --- |
| *(none)* | Series X profile, 2560x1440, DLSS Quality, no frame-rate cap |
| `-FrameGen`, `-FrameGenX 4` | frame generation from the start, 2x to 6x |
| `-Dlss off` / `dlaa` / `balanced` / `performance` | the title's own upscaler, or another DLSS mode |
| `-Fps 60` | frame-rate cap (30 is the title's own) |
| `-SeriesS` | Series S profile: 1280x720 internally, faster, softer |
| `-Res 1080to1440` / `4k` | 1080p scene upscaled by the title / the stock 3840x2160 output |
| `-FgType auto` / `dlss` / `fsr` | which frame generation to use |
| `-Build diag` | diagnostics build (slower; captures and timelines) |
| `-More @{VICTORY_X='1'}` | add or override any switch |

**Switches.** Every behaviour switch is a `VICTORY_*` variable. The launcher
applies `%LOCALAPPDATA%\victory\victory-settings.txt` (one `NAME=value` per line)
on every start and **ignores variables from the outer environment**, so set
switches in that file. Delete it to go back to the defaults.

**Shader cache across builds.** Translated shaders and pipelines are cached by a
fingerprint of the shader translator. `crates/victory-gpu/translator.pin` pins
that fingerprint, so rebuilding does not re-translate everything. **Delete the
file whenever the shader translator (`crates/victory-shader`) really changes**,
or the old translations will be reused.

---

## Performance on an RTX 4070

Measured on Windows 11, Ryzen 7 5700X, 32 GB RAM and a **GeForce RTX 4070
(12 GB)**, using the Series S profile, always after the warm-up restart.

### Where the frame time actually goes

Tuning shaders and drivers did not move the frame rate. We used Nsight Graphics
GPU Trace (with range markers), Nsight Systems and the layer's own telemetry to
measure where the time goes:

| Measurement | Result |
| --- | --- |
| GPU shader units (SMs) busy | only **~9–11%** of the frame |
| GPU front end waiting for idle | **~45%** of the frame |
| Where those waits come from | **~81%** inside the title's own command stream: ~700 global UAV barriers a frame, cheap on the console's AMD GPU, a full drain on NVIDIA |
| CPU preparing each frame | **~45 ms of fixed memory-coherence work** (copies, refreshes, write tracking), even when looking at a wall. This caps the game at ~20 fps in any scene |
| Guard-page faults on the title's threads | ~430–560 a frame, ~50–65 ms of CPU time |
| Ruled out by measurement | mesh-shader output counts, resolution, PCIe reads, texture inflation, shader count |

**The root cause.** The console has unified memory. On a discrete GPU, the layer
keeps two copies of the title's memory in sync, and the work of keeping them in
sync is what binds the frame. That is why resolution and upscaling barely change
the frame rate.

### What is on by default

| Switch | What it does | Turn it off |
| --- | --- | --- |
| `VICTORY_RF_X9` | New submission backend: refreshes, texture uploads and fills run on parallel workers; a fence-retired upload ring; descriptors and samplers synced at submission. Helps most while moving | `VICTORY_RF_X9=0` |
| `VICTORY_RF_X7_NARROW` | Narrows the layer's own barriers to specific resources where that is provably safe. Steadier frame times | `VICTORY_RF_X7_NARROW=0` |

Other experiments stay opt-in and serve measurement or comparison:
`VICTORY_RF_X7_TITLE_UAV`, `VICTORY_RF_X7_VALIDATE`, the X1/X3/X4/X5/X6 switches
and every `*_STATS` switch. Each one is documented in [docs/refactor/](docs/refactor/).

### Current numbers (Series S profile)

| Scene | Before | Now |
| --- | --- | --- |
| Heavy area, standing | ~12–16 fps | **~18–18.5 fps** (~54 ms) |
| Facing a wall (CPU ceiling) | ~19.5 fps | ~19–20 fps |
| Driving (streaming) | ~9.5 fps | **~12–14 fps** |

### Roadmap

- **Finish X9:** give each memory range an owner (GPU-only, CPU-streamed or
  static) so data no longer has to be copied and checked every frame, and turn
  submission into a pipeline that prepares the next submission while the current
  one runs. This is the path to removing the ~45 ms CPU ceiling.
- **The title's barriers:** once the CPU stops being the limit, safely merge and
  drop the GPU drains caused by the title's UAV barriers.

---

## Known issues

- **No ray tracing.** The title's ray tracing is off by default. `VICTORY_BVH_BUILDER=1`
  turns it on, but it is very slow (~3 fps) and the picture breaks. An estimate
  stands in for indirect light and reflections.
- **The first run of every new build is slow** until the automatic warm-up
  restart.
- **Frame generation needs VRAM headroom.** On a full 12 GB card it may run
  unevenly.
- **Frames are only generated while the game window has focus.**
- **SAVE AND RESTART NOW** can leave the old process behind without a window; it
  goes away when the PC restarts.
- **High RAM use.** The layer keeps a host copy of much of the title's memory.
  32 GB is the minimum, and close-to-full RAM causes stutters.

---

## Documentation

- [docs/refactor/](docs/refactor/): each RTX 4070 performance experiment (X1–X9),
  with its contracts, measurements, risks and test scripts.
- [README_AUDITORIA_GRAFICA.md](README_AUDITORIA_GRAFICA.md): graphics audit of the
  layer on the RTX 4070 (Portuguese).
- [README_GTA6_SCARLETT.md](README_GTA6_SCARLETT.md): the title's files and
  architecture under the layer (Portuguese).
- [docs/architecture.md](docs/architecture.md): the layer's parts.
- [docs/findings.md](docs/findings.md): measurements behind earlier choices.
- [docs/dlss.md](docs/dlss.md): DLSS and frame generation.
- [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md): the title's ray tracing
  and the light estimate.
- [docs/README-foxhound.md](docs/README-foxhound.md): the README of foxhound, the
  base this fork has stood on since 2026-11-26.

---

## Credits

A tree of other people's work with some on top; the history is kept.

- **[foxhound](https://github.com/readmet3xt-lab/foxhound)** by readmet3xt-lab:
  the base of this fork since 2026-11-26.
- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and contributors:
  the loader, runtime services, Direct3D 12 back end and shader recompiler.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)**,
  **[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)**,
  **[codec-d/cool_fox](https://github.com/codec-d/cool_fox)**,
  **[DexNom/VIctory](https://github.com/DexNom/VIctory)**,
  **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**
  and the other projects foxhound's README names. Each was built, run and measured
  on the same NVIDIA PC before choosing what to keep.
- Everyone who sent pull requests to this fork.
- NVIDIA Streamline, DLSS and Reflex are NVIDIA's; they are loaded at run time
  from a folder you supply.
- Much of this fork's code was written with [Claude Code](https://claude.com/claude-code).

---

## License

GPL-3.0-or-later, like VIctory and foxhound. See [LICENSE](LICENSE). The one file
not covered is named in [docs/asset-policy.md](docs/asset-policy.md).
