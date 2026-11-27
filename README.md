# Tequesta

**Tequesta** is a custom Rust-based High-Level Emulation (HLE) compatibility layer and D3D12.X → D3D12 translation runtime built to run the Xbox Series (Scarlett) build of **Grand Theft Auto VI (GTA VI)** natively on Windows 10/11, specifically engineered for **NVIDIA RTX GPUs**.

> Named after the indigenous Tequesta people of southeastern Florida and Biscayne Bay.

## Current Status & Benchmarks (GTA VI)

Tested on **Microsoft Windows 11 Famille (x64)** — **AMD EPYC 7543P 32-Core Processor**, **28 GB RAM**, **NVIDIA RTX A4500 (20 GB VRAM, Driver 32.0.15.9686)**:

| Milestone | Status | Verified Notes |
| :--- | :--- | :--- |
| **Executable Load & Boot** | Working | Native x86-64 PE/XEX mapping with TLS redirection, GDK/Scarlett HLE, and `victory recomp` `.pdata` / Zen 2 static lifting scaffolding |
| **Main Menu** | Working | Full 2D/UI rendering at **30 FPS** |
| **In-Game 3D World** | Working | Boots into Lucia's opening prologue (café cutscene & prison sequence before gaining control of Lucia) |
| **Playable & Controls** | Working | Full character movement in the prison and subsequent scenes with working HUD, pause menu, AZERTY/QWERTY keyboard support (including AZERTY scancode `0x10` cover `RB` and `E` interaction), and a dedicated `125 Hz` `RawMousePump` (`WM_INPUT` + cursor-delta fallback, auto-captured on focus when `F1` is closed, `F2`/`~` toggle, `Esc`/`F1` unlock) |
| **In-Game Frame Rate** | **5–27 FPS** | Measured in active 3D scenes with `VICTORY_PORT_STATIC=1` on RTX A4500 |
| **3D Lighting & Shaders** | Partial | Deferred lighting, shadows, and environment geometry render; startup shader prewarm and pipeline caching enabled by default, with live `F1` toggles for ambient light and reflection guard |
| **Known Issues** | Active | Facial morph targets (`Dispatch` blendshapes) remain static during cutscenes; minor BC7/SSS skin detiling artifacts on character faces/forearms |

> **Video Proof:** [GTA VI In-Game on NVIDIA RTX A4500 (earlier 6–12 FPS capture)](https://www.youtube.com/watch?v=V8ioeQRAytM)

## Architecture Overview

Unlike traditional CPU emulators, x86-64 game instructions execute natively on the Windows host:

- **`victory-loader`**: Maps the target executable and libraries into host memory, redirects static TLS, binds imports to native Windows DLLs or HLE modules, and provides `victory_loader::recomp` (`.pdata` function blocks, Zen 2 `MONITORX`/`MWAITX`/`CLWB`/`CLZERO` instruction scanning, x86-64 import trampolines, and static binary patching via `victory recomp`).
- **`victory-hle`**: Implements Scarlett / GDK OS interfaces, DirectStorage streaming (`256 MiB` default staging), XMA audio decoding, `WaitOnAddress` fence synchronization, `125 Hz` `RawMousePump` / `MouseLook` smoothing, and PC keyboard/mouse input mapping (`VICTORY_KBM_GTA=1`, `VICTORY_KBM_NATIVE=1`).
- **`victory-gpu`**: Translates D3D12.X command lists, descriptors, unified memory tracking (card rehash & Pareto residency), and Xbox BCn texture detiling / DXIL shaders to standard Windows D3D12, plus the `760x660` Vice-styled `F1` overlay (`tequesta_ui.rs`).

## Key NVIDIA RTX & Performance Optimizations

- **NVIDIA Exact Arena Sub-Allocation (`ExactArenaPool`)**: Replaces slow individual `CreateCommittedResource` calls on NVIDIA drivers (`VICTORY_RAW_VIEWS=0`) with 256-byte aligned 4 MiB sub-allocated arenas, eliminating driver contention during 3D scene submission.
- **Static Command List Caching (`VICTORY_PORT_STATIC=1`)**: Memoizes recurring static geometry command sequences, caches descriptor table lookups (`33,300+` direct hits/frame), skips redundant constant rebinds (`1,470–1,559`/frame), and batches indirect draw translations (`VICTORY_THIN=1`, `VICTORY_JOIN_PARTS=8`), lifting interior 3D scene performance on RTX A4500 from `~5.7 FPS` to `7.8–8.5 FPS`.
- **Startup Shader Prewarm & Pipeline Caching (`shader_prewarm` / `pipeline_precache`)**: Enabled by default in `tequesta_config.toml` to run non-blocking background shader prewarming and populate `ID3D12PipelineLibrary1` / PSO caches at launch.
- **Slab/Bitset Descriptor Caching**: Eliminates per-frame `CopyDescriptorsSimple` bottlenecks across high-draw-count frames (`3,000–5,500+` draws/frame).
- **Pareto Memory Residency & Batched Card Rehashes**: Prevents WDDM/PCIe paging stalls and accelerates GPU memory tracking (`VICTORY_CARD_REHASH_RUNS=1`).
- **Low-Overhead Synchronization**: Uses native Windows `WaitOnAddress` / `WakeByAddressAll` for blocking fence waits instead of busy-spin fallbacks.
- **Native D3D12 Overlay (`F1`) & Config Persistence**: Built-in `760x660` Vice-styled `F1` menu (`tequesta_ui.rs`) persisted in `tequesta_config.toml` with live `Borderless` / `Windowed` toggle (`WM_APP_DISPLAY_MODE`), `VICTORY_PORT_STATIC`, FPS cap, mouse sensitivity (`0.25x` to `8.00x`, default `2.00x`), ambient & reflection guard toggles, FPS/ms HUD, plus launch options (`Resolution Scale`, `Native Keyboard & Mouse`, `Parallel Command Lists`, `Translation Workers`, `Direct GPU Views`, `Shader Preload`, `Pipeline Cache`).

## Build & Run

### Step-by-Step Setup Guide (Beginners)

#### 1. Install Build Prerequisites (Windows 10/11 x64)
1. **Install Visual Studio Build Tools (C++):** Download and run the [Visual Studio Build Tools installer](https://visualstudio.microsoft.com/visual-cpp-build-tools/) and select the **Desktop development with C++** workload (MSVC linker + Windows 10/11 SDK).
2. **Install Rust (MSVC toolchain):** Run `rustup-init.exe` from [rustup.rs](https://rustup.rs/) (`stable-x86_64-pc-windows-msvc`), open a new PowerShell terminal, and verify `rustc --version` and `cargo --version`.
3. **Update NVIDIA Drivers:** Install up-to-date DirectX 12 / Shader Model 6.6+ NVIDIA drivers.

#### 2. Clone & Compile Tequesta (Release Build)
```powershell
git clone https://github.com/0v3rr/Tequesta.git && cd Tequesta
cargo build --release --locked -p victory
```
The compiled binary is written to `target\release\victory.exe`.

#### 3. Prepare Your Local Game Directory
1. Place your legally dumped Xbox Series (Scarlett) build files (`game_scarlett_master.exe` plus its data and shader archives) into a local folder (for example, `D:\Games\GTAVI_Scarlett\`).
2. *(Optional)* Pre-translate RDNA2 shaders to DXIL and warm up the D3D12 pipeline cache before launching:
```powershell
.\target\release\victory.exe --precompile-shaders "D:\Games\GTAVI_Scarlett"
```

#### 4. Launch & Configure In-Game (`F1` Overlay)
1. Launch with the recommended NVIDIA RTX environment flags:
```powershell
$env:VICTORY_PORT_STATIC="1"; $env:VICTORY_THIN="1"; $env:VICTORY_JOIN_PARTS="8"; $env:VICTORY_CARD_REHASH_RUNS="1"; $env:VICTORY_KBM_NATIVE="1"; $env:VICTORY_RAW_VIEWS="0"; .\target\release\victory.exe run --execute "D:\Games\GTAVI_Scarlett\game_scarlett_master.exe"
```
2. Wait through the initial shader/pipeline warm-up and opening prologue until Lucia is controllable, then press **F1** to open the overlay (`tequesta_config.toml` is saved automatically):
   - Keep **DIRECT GPU VIEWS** = **OFF** (`VICTORY_RAW_VIEWS=0`), **SHADER PRELOAD** = **ON**, and **PIPELINE CACHE** = **ON**.
   - For higher FPS or lower VRAM usage, set **RESOLUTION SCALE** to **75%** or **50%** and click **APPLY & RESTART**.

### Prerequisites
- Windows 10/11 (x64) with a DirectX 12 capable GPU (tested on **NVIDIA RTX A4500**)
- Rust toolchain (`stable-x86_64-pc-windows-msvc`) & Visual Studio C++ Build Tools
- Legally obtained/local Xbox Series build files (no game files, keys, or precompiled binaries are included in this repository)

## Controls & Hotkeys (`VICTORY_KBM_NATIVE=1`)

Tequesta translates native Windows `WM_INPUT` raw mouse movement and layout-independent keyboard scancodes (supporting both **QWERTY** and **AZERTY** layouts out of the box):

| Action | Keyboard / Mouse Input | Xbox Series Equivalent |
| :--- | :--- | :--- |
| **Open / Close Overlay Menu** | `F1` | Releases mouse cursor & opens `tequesta_ui` |
| **Toggle Mouse Camera Capture** | `F2` or `~` (`²` on AZERTY, scancode `0x29`) | Auto-captures on window focus when `F1` is closed; `F2`/`~` toggles, `Esc`/`P`/`F1` releases |
| **Move** | `W` `A` `S` `D` (QWERTY) / `Z` `Q` `S` `D` (AZERTY) | Left Stick |
| **Camera Look** | Mouse Movement | Right Stick |
| **Fire / Attack / Accelerate** | Left Mouse Click | `RT` |
| **Aim / Brake / Reverse** | Right Mouse Click | `LT` |
| **Look Behind / Special** | Middle Mouse Click | `R3` |
| **Sprint** | `Shift` | `A` |
| **Jump / Climb** | `Space` | `X` |
| **Reload / Melee** | `R` | `B` |
| **Enter / Exit Vehicle** | `F` | `Y` |
| **Interact / Talk** | `E` | `D-Pad Right` |
| **Cover / Handbrake** | `Q` (QWERTY) / `A` (AZERTY, scancode `0x10`) | `RB` |
| **Weapon Wheel** | `Tab` | `LB` |
| **Crouch / Stealth** | `Ctrl` | `L3` |
| **Pause Menu** | `Esc` or `P` | Menu (also frees mouse cursor) |

> **Gameplay & Mouse Look Note:** During the opening prologue, the game plays through an introductory cutscene and prison sequence before handing over full control of Lucia. Once focused with the `F1` overlay closed, raw mouse camera look (`125 Hz` `RawMousePump` via `WM_INPUT`, with cursor-delta fallback) engages automatically and clips the cursor to the client area; press `F2` or `~` to toggle capture, or `F1` / `Esc` / `P` to release the cursor.

## Configuration & Environment Variables

### In-Game Overlay Menu (`F1`) & `tequesta_config.toml`
Pressing `F1` opens the native D3D12 settings overlay (`tequesta_ui.rs`). Changes are persisted in `tequesta_config.toml`:

- **Live Settings (applied immediately without restart):**
  - `Display Mode`: `Borderless` / `Windowed` (live window style & `GetClientRect` hit-testing update)
  - `Mouse Sensitivity`: `0.25x` to `8.00x` (default `2.00x`)
  - `Frame Rate Cap`: `Unlimited` / `30 FPS` / `60 FPS`
  - `Static Draw Cache` (`VICTORY_PORT_STATIC`): Toggle static command-list & descriptor table caching (`ON` / `OFF`)
  - `Ambient Indirect Light`: Toggle indoor indirect lighting pass (`ON` / `OFF`)
  - `Reflection & Probe Guard` (`VICTORY_REFLECTION_GUARD`): Toggle volumetric fog / probe stabilization (`ON` / `OFF`)
  - `Performance HUD`: Compact top-right FPS & frame-time overlay (`ON` / `OFF`)
- **Restart-Applied Settings (saved to `tequesta_config.toml` via *Apply & Restart*):**
  - `Resolution Scale`: `50%` / `75%` / `100%`
  - `Native Keyboard & Mouse` (`VICTORY_KBM_NATIVE`): Toggle native `WM_INPUT` raw mouse & AZERTY/QWERTY scancodes (`ON` / `OFF`)
  - `Parallel Command Lists`: Toggle multi-threaded command list translation (`ON` / `OFF`)
  - `Translation Workers`: `1` / `4` / `8` worker threads
  - `Direct GPU Views` (`VICTORY_RAW_VIEWS`): Toggle raw committed resources vs `ExactArenaPool` (`OFF` / `ON`, keep `OFF` on NVIDIA GPUs)
  - `Shader Preload` (`shader_prewarm`): Background shader prewarm pass at startup (`ON` / `OFF`, default `ON`)
  - `Pipeline Cache` (`pipeline_precache`): Startup pipeline library / PSO cache (`ON` / `OFF`, default `ON`)

### Recommended Launch Environment Flags (NVIDIA RTX)

| Variable | Recommended | Purpose |
| :--- | :---: | :--- |
| `VICTORY_PORT_STATIC` | `1` | Enables static draw-state memoization and 512-entry direct descriptor table caching |
| `VICTORY_THIN` | `1` | Merges contiguous `ExecuteIndirect` calls sharing the same signature/buffer |
| `VICTORY_JOIN_PARTS` | `8` | Parallel command list translation worker count |
| `VICTORY_CARD_REHASH_RUNS` | `1` | Batches unified GPU memory card-table rehashes |
| `VICTORY_KBM_NATIVE` | `1` | Enables native `WM_INPUT` raw mouse and scancode keyboard input |
| `VICTORY_RAW_VIEWS` | `0` | Keeps 4 MiB `ExactArenaPool` sub-allocation active (essential for NVIDIA drivers) |
| `VICTORY_WINDOW_TITLE` | `Tequesta - Grand Theft Auto VI` | Optional custom window caption override |

### Offline Shader & Pipeline Precompilation
Pre-translate RDNA2 shaders to DXIL in parallel and populate the `ID3D12PipelineLibrary1` cache before launching:

```powershell
cargo run --release --locked -p victory -- --precompile-shaders "<PATH_TO_GAME_DIR>"
```
