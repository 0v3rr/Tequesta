# Tequesta

**Tequesta** is a custom Rust-based High-Level Emulation (HLE) compatibility layer and D3D12.X → D3D12 translation runtime built to run the leaked Xbox Series (Scarlett) build of **Grand Theft Auto VI (GTA VI)** natively on Windows 10/11, specifically engineered for **NVIDIA RTX GPUs**.

> Named after the indigenous Tequesta people of southeastern Florida and Biscayne Bay.

---

## Current Status & Benchmarks (GTA VI)

Tested on **Windows 10/11 (x64)** with an **NVIDIA RTX A4500 (20 GB VRAM)**:

| Milestone | Status | Verified Notes |
| :--- | :--- | :--- |
| **Executable Load & Boot** | Working | Native x86-64 PE/XEX mapping with TLS redirection and GDK/Scarlett HLE |
| **Main Menu** | Working | Full 2D/UI rendering at **30 FPS** |
| **In-Game 3D World** | Working | Boots into Lucia's opening prologue (café cutscene & prison cell) |
| **Playable & Controls** | Working | Full character movement in the prison and subsequent scenes with working HUD, pause menu, and keyboard/mouse input (VICTORY_KBM_GTA=1) |
| **In-Game Frame Rate** | **6–12 FPS** | Measured in active 3D scenes with `VICTORY_PORT_STATIC=1` on RTX A4500 |
| **3D Lighting & Shaders** | Partial | Deferred lighting, shadows, and environment geometry render; initial scene streaming warms up during the opening cutscene |
| **Known Issues** | Active | Facial morph targets (`Dispatch` blendshapes) remain static during cutscenes; minor BC7/SSS skin detiling artifacts on character faces/forearms |

> **Video Proof:** [GTA VI In-Game on NVIDIA RTX A4500 (6–12 FPS)](https://www.youtube.com/watch?v=V8ioeQRAytM)

---

## Architecture Overview

Unlike traditional CPU emulators, x86-64 game instructions execute natively on the Windows host:

- **`vectory-loader`**: Maps the target executable and libraries into host memory, redirects static TLS, and binds imports to native Windows DLLs or HLE modules.
- **`vectory-hle`**: Implements Scarlett / GDK OS interfaces, DirectStorage streaming (`256 MiB` default staging), XMA audio decoding, `WaitOnAddress` fence synchronization, and PC keyboard/mouse input mapping (`VICTORY_KBM_GTA=1`).
- `**victory-gpu`**: Translates D3D12.X command lists, descriptors, unified memory tracking (card rehash & Pareto residency), and Xbox BCn texture detiling / DXIL shaders to standard Windows D3D12.

---

## Key NVIDIA RTX & Performance Optimizations

- **NVIDIA Exact Arena Sub-Allocation (`ExactArenaPool`)**: Replaces slow individual `CreateCommittedResource` calls on NVIDIA drivers (`VICTORY_RAW_VIEWS=0`) with 256-byte aligned 4 MiB sub-allocated arenas, eliminating driver contention during 3D scene submission.
- **Static Command List Caching (`VICTORY_PORT_STATIC=1`)**: Memoizes recurring static geometry command sequences and batches indirect draw translations (`VICTORY_THIN=1`, `VICTORY_JOIN_PARTS=8`), lifting 3D scene performance from ~5 FPS to **10–12 FPS**.
- **Slab/Bitset Descriptor Caching**: Eliminates per-frame `CopyDescriptorsSimple` bottlenecks across high-draw-count frames (`3,000–5,500+` draws/frame).
- **Pareto Memory Residency & Batched Card Rehashes**: Prevents WDDM/PCIe paging stalls and accelerates GPU memory tracking (`VICTORY_CARD_REHASH_RUNS=1`).
- **Low-Overhead Synchronization**: Uses native Windows `WaitOnAddress` / `WakeByAddressAll` for blocking fence waits instead of busy-spin fallbacks.

---

## Build & Run

### Prerequisites
- Windows 10/11 (x64) with a DirectX 12 capable GPU (tested on **NVIDIA RTX A4500**)
- Rust toolchain (`stable-x86_64-pc-windows-msvc`) & Visual Studio C++ Build Tools
- Legally obtained/local XBox Series build files (no game files, keys, or precompiled binaries are included in this repository)

### Build from Source
```powershell
cargo build --release --locked -p victory
```

### Run
```powershell
$env:VICTORY_THIN = "1"
$env:VICTORY_PORT_STATIC = "1"
$env:VICTORY_JOIN_PARTS = "8"
$env:VICTORY_CARD_REHASH_RUNS = "1"
$env:VICTORY_KBM_GTA = "1"
.\target\release\victory.exe <path-to-game-executable>
```

---

## Disclaimer

This repository contains **source code only**. No copyrighted game assets, encryption keys, or precompiled executables are hosted, distributed, or linked. Licensed under **GPL-3.0**.



