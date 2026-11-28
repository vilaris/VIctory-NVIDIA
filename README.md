# VIctory on NVIDIA

This fork of VIctory runs the Xbox Series build of Grand Theft Auto VI on Windows as a compatibility layer. It translates the title's graphics work to Direct3D 12 and is tuned for NVIDIA GeForce RTX GPUs. It supports Series X and Series S profiles, DLSS upscaling, and optional frame generation.

> **Bring your own legally obtained dump.** The repository distributes no game executable, code, or assets. This independent project is unaffiliated with Rockstar Games, its publisher, and NVIDIA. See the [asset policy](docs/asset-policy.md).

## Quick start

1. Get `victory.exe` from the [fork's Releases page](https://github.com/vilaris/VIctory-NVIDIA/releases) when a binary is available. You can also [build it from source](#build-from-source).
2. Put it beside the original `game_scarlett_master.exe` from your Xbox Series v1.0.2.1 dump.
3. Double-click `victory.exe`, then advance past the title screen into the game.

You can keep the launcher in a separate folder and pass the full path to the game executable.

```powershell
.\victory.exe play 'D:\Games\MyDump\game_scarlett_master.exe'
```

A fresh configuration starts in the Series X profile at 2560×1440, with DLSS Quality when the NVIDIA files and GPU support it. The launcher prepares frame generation where possible but starts with it off. It can offer to download missing NVIDIA or AMD files. On RTX 20/30 with DLSS enabled, it can also offer the optional MFG Unlocker mod.

**Warm-up restart.** The built-in launcher normally restarts the game once for each distinct `victory.exe` binary. The 30-second timer starts at the first frame with at least 100 draws, after you advance past the title screen. Before restarting, it writes a marker at `%LOCALAPPDATA%\victory\warmed\<hash>`, using the first 16 hex digits of the executable's BLAKE3 hash. A custom `VICTORY_HOME` moves that folder. Set `VICTORY_WARMUP_RESTART=0` in the settings file to skip this behavior.

### Launcher commands

| Command | What it does |
| --- | --- |
| `victory.exe play [EXE]` | Starts the game with the built-in launcher settings. With no path it looks beside the launcher, then in the current directory. |
| `victory.exe get-dlss` | Downloads NVIDIA Streamline and the DLSS files into the launcher's `nvidia-sdk` folder. |
| `victory.exe get-d3d12` | Downloads Microsoft's Agility SDK runtime into `D3D12` beside the executable. |
| `victory.exe get-fsr` | Downloads the AMD FidelityFX DLLs into the launcher's `amd-fidelityfx` folder. |
| `victory.exe get-mfg` | Downloads the third-party MFG Unlocker into the launcher's `mfg-unlocker` folder. Enable it in F1 and restart to load it. |
| `victory.exe preload [PATHS]...` | Translates shaders and prepares pipelines. `play --preload` does this before starting the game. |
| `victory.exe upscaler-check` | Checks NVIDIA Streamline, DLSS, and DLSS Frame Generation without opening the game. |

Use `victory.exe --help` for the full command list and `victory.exe play --help` for the launch options.

## Requirements

| Component | What to use |
| --- | --- |
| Operating system | 64-bit Windows 10 or 11. Windows 10 needs the D3D12 Agility SDK for this title. |
| CPU | x86-64 with AVX2. The runtime checks for AVX2 before executing the title. |
| GPU | A Direct3D 12 feature level 12_0 GPU with Shader Model 6.6, directly indexed descriptor heaps, and mesh shader support for the title's mesh pipelines. Use a current graphics driver. A GeForce RTX GPU is required for the DLSS path. |
| RAM | 32 GB is the tested configuration for the measurements below. The texture upload code limits staging on systems with less than 24 GiB of physical RAM. |
| Game files | Your own Xbox Series v1.0.2.1 dump. Use the unmodified retail executable for the 1440p patch and DLSS. |

On Windows 10, `play` can fetch Agility SDK 1.614.1 from Microsoft's NuGet package. It installs `D3D12\D3D12Core.dll` beside `victory.exe`. Use `get-d3d12` to install it manually. Windows 11 normally uses its built-in runtime. If a local Agility SDK DLL is present, the launcher reads its SDK version.

## Frame generation by GPU

| GPU | Frame generation available in this branch |
| --- | --- |
| GeForce RTX 50 | DLSS Frame Generation up to 6×, subject to the GPU and driver's reported limit. FSR 3 is an optional 2× mode. |
| GeForce RTX 40 | DLSS Frame Generation at 2×, or optional FSR 3 at 2×. |
| GeForce RTX 20/30 | FSR 3 at 2× with DLSS upscaling. The third-party MFG Unlocker can also be tried for DLSS Frame Generation. It is off by default. |
| GPU without DLSS | Frame generation is unavailable in this branch. Its FSR 3 integration requires active DLSS upscaling in both console profiles. |

DLSS Frame Generation needs hardware-accelerated GPU scheduling. In F1, **FRAME GEN TYPE** selects **AUTO**, **DLSS**, or **FSR 3**. Auto prefers DLSS Frame Generation where available and can fall back to FSR 3. **FRAME GENERATION** selects the multiplier or turns it off. The startup overlay reports the available mode or why it could not start. [DLSS and frame generation details](docs/dlss.md)

## In the game

**F1** opens and closes the settings menu while the game has focus. Use ↑/↓ to move, ←/→ to change a value, Enter to select or activate, and Esc to close.

| Settings | Menu items |
| --- | --- |
| Upscaling and frame generation | DLSS, DLSS Model, Frame Generation, Frame Gen Type, MFG Unlocker |
| Display and input | Frame Rate Cap, FPS at the Top, Stats Panel (F3), Fullscreen (No Border), Mouse Capture, English Keyboard |
| Console profile | Console (Series X or Series S), Series X Picture (2560×1440, 1080p to 1440p, or 3840×2160) |
| Graphics | Light Estimate, Reflection Estimate, Fog Cap, Ray Tracing, Water Light, Heat Haze and Decal Defrag, Early Depth Test |
| Performance tuning | Soft Views, Texture No Wait, Relay, Card Steps, Indirect Batch, Card Compares From (KB) |

The menu saves changes as you make them. The FPS line and stats panel update immediately, as does the frame-generation multiplier when frame generation was prepared at launch. Use **SAVE AND RESTART NOW** to apply the other settings immediately or **SAVE FOR NEXT START** to apply them later. **PRELOAD SHADERS (RESTARTS THE GAME)** asks for confirmation and prepares shaders and pipelines before the next launch. **RESET ALL TO PLAY DEFAULTS** clears saved settings.

**F3** toggles the stats panel. It shows rendered FPS, average and longest frame time, estimated displayed FPS with frame generation, draw counts, submission and GPU timing, copy-back time, upload and texture volumes, process RAM, and VRAM usage and budget.

## Build from source

Install Rust through rustup and Visual Studio Build Tools with the **Desktop development with C++** workload. The repository selects its Rust version and the Windows MSVC target in [rust-toolchain.toml](rust-toolchain.toml).

```powershell
git clone https://github.com/vilaris/VIctory-NVIDIA.git
cd VIctory-NVIDIA
cargo build --release --locked -p victory
.\target\release\victory.exe play 'D:\Games\MyDump\game_scarlett_master.exe'
```

The [PowerShell launcher](scripts/play-nvidia.ps1) forwards its arguments to [play-foxhound.ps1](scripts/play-foxhound.ps1). It builds the selected binary if it is missing, then starts the title with `victory run --execute`. For this launcher, run [Get-NVIDIA-DLSS.cmd](Get-NVIDIA-DLSS.cmd) for NVIDIA files and [Get-AMD-FSR.cmd](Get-AMD-FSR.cmd) if you need FSR 3. On Windows 10, install Agility beside the selected binary with its `get-d3d12` command before launching through the script.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\play-nvidia.ps1 -Exe 'D:\Games\MyDump\game_scarlett_master.exe' -SeriesS
```

| Script option | Values and default |
| --- | --- |
| `-Exe` | Required path to the original game executable. |
| `-Build` | `release` by default, or `diag` to build with the diagnostics feature. |
| `-SeriesS` | Off by default (Series X). Selects the Series S profile with a 1280×720 internal scene. |
| `-Res` | Series X only. `1440` (default) for a 2560×1440 scene and output, `1080to1440` for a 1920×1080 scene upscaled by the title, or `4k` for the stock 3840×2160 output. |
| `-Dlss` | `quality` by default. Also accepts `off`, `dlaa`, `balanced`, and `performance`. |
| `-FrameGen` | Off by default. Enables frame generation at startup when supported. |
| `-FrameGenX` | Requested multiplier from 2 to 6, default 2. Use with `-FrameGen`. |
| `-NoFrameGenReady` | Off by default. With this switch, the script prepares frame generation only if it is enabled for startup. |
| `-FgType` | `auto` by default. Also accepts `dlss` and `fsr`. |
| `-Fps` | 1 to 1000. The default of 1000 is effectively uncapped. The title's original cap is 30. |
| `-Preload` | Off by default. Prepares shaders and pipelines before launch. |
| `-More` | PowerShell hashtable of overrides, empty by default. For example, `-More @{VICTORY_RF_X9='1'}`. |

**Shader cache.** A nonempty [translator.pin](crates/victory-gpu/translator.pin) supplies the translator fingerprint and allows cached translations to survive rebuilds. If you change the translator, remove the pin before building so the build script fingerprints the source again and invalidates stale translations.

## Switches and settings

Edit `victory-settings.txt` with one `VICTORY_NAME=value` per line. Use an empty value to unset a switch. To restore the launcher's defaults, remove the file.

| How you start the game | Settings file | Override order |
| --- | --- | --- |
| Double-click or `victory.exe play` | `%LOCALAPPDATA%\victory\victory-settings.txt`, or under `VICTORY_HOME` | Launch options, then saved settings, then repeated `--set NAME=VALUE` overrides for switches. |
| `scripts\play-nvidia.ps1` or `scripts\play-foxhound.ps1` | `victory-settings.txt` in the repository root | Script options, then saved settings, then `-More` overrides for switches. |

The launchers clear inherited `VICTORY_*` performance switches before applying their settings. They preserve selected path variables such as `VICTORY_CACHE_DIR` and `VICTORY_DATA_DIR`. Use the settings file for changes you want to carry across F1 restarts.

| Performance switch | Default | Effect |
| --- | --- | --- |
| `VICTORY_RF_X9` | Off | Experimental coherence and submission backend. It lowers CPU preparation but currently adds GPU waits, so it is a net loss on the RTX 4070 (16–18 FPS on vs 18–20 off). Set `VICTORY_RF_X9=1` to test it. |
| `VICTORY_RF_X7_NARROW` | On | Narrows eligible barriers emitted by the compatibility layer. Set `VICTORY_RF_X7_NARROW=0` to disable it. |
| `VICTORY_RF_X7_TITLE_UAV` | Off | Experiments with omitting redundant global UAV waits in the title's command stream. |
| `VICTORY_RF_X7_VERIFY`, `VICTORY_RF_X7_VALIDATE`, `VICTORY_RF_X9_VERIFY` and `VICTORY_RF_*_STATS` | Off | Enable the corresponding checks or telemetry explicitly. X9 verification also requires X9 to be enabled. |
| `VICTORY_RF_X1_PRIORITY` | Off | Prioritizes card memory for eligible GPU resources. |
| `VICTORY_RF_MARKERS`, `VICTORY_RF_X2_MEMSTATS` | Off | GPU trace markers (`1` or `2`) and memory statistics (`1`). Markers also enable statistics. |
| `VICTORY_RF_X3_FASTSKIP`, `VICTORY_RF_X3_VERIFY` | Off | Skips eligible clean refreshes. Verification also requires fast-skip. |
| `VICTORY_RF_X4_PICK_GATE`, `VICTORY_RF_X4_PICKED`, `VICTORY_RF_X4_VERIFY` | Off | Descriptor refresh experiments and verification. |
| `VICTORY_RF_X5_ALWAYS_DIRTY`, `VICTORY_RF_X5_LAZY_REARM`, `VICTORY_RF_X5_FAST_FAULT` | Off | Write-tracking and guard fault experiments. Fast-fault handling defaults on when the always-dirty experiment is enabled. |
| `VICTORY_RF_X6_RESTORE_LEFTOUT` | Off | Restores graphics passes omitted by the launcher. |

The [refactor notes](docs/refactor/) describe the individual switches and measurements.

## Performance work on the RTX 4070

The Series S investigations used an RTX 4070, a Ryzen 7 5700X, and 32 GB of RAM. GPU traces and submission telemetry exposed two limits.

| Measurement | Finding |
| --- | --- |
| GPU front end | Wait-For-Idle consumed about 45% of GPU time, with SM utilization around 9–11%. About 81% of the waits attributed to leaf ranges fell inside the title's command ranges. The title emitted hundreds of global UAV barriers per frame, a major source of GPU serialization in these captures. |
| CPU submission | Coherence and submission preparation took about 45 ms per frame even while facing a wall. The layer tracks writes and keeps host memory and GPU copies coherent on a discrete GPU. Driving adds more streaming, refresh, and texture-upload work. |
| Guard-page faults | Two Series S runs measured 426–465 faults and 51–54 ms of fault-handler time per frame across the title's threads. That accumulated time can overlap execution on other threads. |

Mesh output counts, resolution, PCIe reads, and texture inflation were investigated and ruled out as primary explanations for the measured limit. See the [synchronization trace](docs/refactor/x2-sincronizacao.md), [barrier analysis](docs/refactor/x7-barreiras.md), [fault measurements](docs/refactor/x5-submissao.md), and [graphics audit](docs/auditoria-grafica/).

### Current numbers

Rendered frame rates on the Series S setup above, with the defaults (X7 narrowing on, X9 off), measured after the warm-up restart.

| Scene | Rendered FPS |
| --- | ---: |
| Standing still in a heavy scene | ~18–20 |
| Facing a wall | ~19–20 |
| Driving | ~12–14 |

**X9 is still in development and off by default.** With `VICTORY_RF_X9=1` the same heavy scene measured 16–18 FPS: CPU preparation drops, but new GPU waits outweigh it. Changes starting at `107c991` once regressed to about 66,000 refreshes per frame; that is fixed (`VICTORY_RF_X9_BINDLESS_INCREMENTAL`, and the bindless descriptor wait moved out of the default path).

X9 already uses bitmap tracking for eligible guard faults, parallel preparation for refreshes and uploads, and a fence-retired upload ring. The next work is to make memory ownership explicit for eligible GPU-only, CPU-streamed, and static ranges, and to overlap preparation of successive submissions while preserving coherence. Removing redundant waits in the title's command stream is a separate GPU task. [X9 implementation and roadmap](docs/refactor/x9-backend.md)

## Known issues

- Ray tracing is disabled by default. The F1 option is experimental and can produce broken rendering at very low frame rates. The default lighting and reflection estimates work without it. [Ray tracing notes](docs/fork-ray-tracing.md)
- Frame generation needs spare VRAM. The launcher can reserve room by limiting texture memory use.
- Shader and pipeline preparation can take time on the first run or after a translator or graphics driver update. Preload can resume from the work already cached.

## Reporting a problem

Include the fork release or commit, Windows version and build, GPU and driver, console profile, and steps to reproduce. Attach the log that matches the affected run. The launcher prints its path on the `Log:` line. [Fork issue tracker](https://github.com/vilaris/VIctory-NVIDIA/issues)

| Log | Default path |
| --- | --- |
| Built-in launcher | `%LOCALAPPDATA%\victory\logs\victory-nvidia-play-<YYYYMMDD>-<HHMMSS>-<PID>.log` |
| PowerShell launcher | `%LOCALAPPDATA%\victory\logs\victory-nvidia-<YYYYMMDD>-<HHMMSS>.log` |
| Crash report, if present | `%LOCALAPPDATA%\victory\data\7952987A\crashes.log` |

If you changed `VICTORY_HOME` or the configured data directory, use the corresponding path for your run.

## Documentation

- [Architecture](docs/architecture.md) and [earlier findings](docs/findings.md)
- [Refactor experiments](docs/refactor/) and [RTX 4070 graphics audit](README_AUDITORIA_GRAFICA.md)
- [Scarlett build notes](README_GTA6_SCARLETT.md)
- [DLSS integration](docs/dlss.md) and [GPU replay for diagnostics builds](docs/replay.md)
- [foxhound documentation](docs/README-foxhound.md) and [asset policy](docs/asset-policy.md)

## Credits

- NVIDIA fork and integration work by [trustno1-coder/VIctory-NVIDIA](https://github.com/trustno1-coder/VIctory-NVIDIA).
- RTX tuning and the current base from [readmet3xt-lab/foxhound](https://github.com/readmet3xt-lab/foxhound).
- Original compatibility layer by Marvisak and contributors. [VIctory](https://github.com/Marvisak/VIctory)
- Related fork work by [fxceless/victory-fork](https://github.com/fxceless/victory-fork), [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/victory-7900xtx), [codec-d/cool_fox](https://github.com/codec-d/cool_fox), [DexNom/VIctory](https://github.com/DexNom/VIctory), and [Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD).
- Optional components from [NVIDIA Streamline](https://github.com/NVIDIA-RTX/Streamline), [AMD FidelityFX SDK](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK), and [dashdogy/MFG Unlocker](https://github.com/dashdogy/MFG-Unlocker).
- Thanks to everyone who contributed patches or reports.

## License

The project code is licensed under GPL-3.0-or-later. Downloaded components retain their respective licenses. The BCPack table exception is documented in the asset policy. [LICENSE](LICENSE) · [Asset policy](docs/asset-policy.md)
