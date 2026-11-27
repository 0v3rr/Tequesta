# Tequesta

**Tequesta** is a custom Rust-based High-Level Emulation (HLE) compatibility layer and D3D12.X → D3D12 translation runtime designed to execute native x86-64 Xbox Series (Scarlett) binaries on Windows 10/11, specifically tuned for NVIDIA RTX GPUs.

> Named after the indigenous Tequesta people of southeastern Florida and Biscayne Bay.

## Architecture Overview

Unlike traditional CPU emulators, x86-64 game instructions execute natively on the Windows host:

- **`victory-loader`**: Maps the target executable and libraries into host memory, redirects static TLS, and binds imports to native Windows DLLs or HLE modules.
- **`victory-hle`**: Implements Scarlett / GDK OS interfaces, DirectStorage streaming (`256 MiB` default staging), XMA audio decoding, and optional PC-style keyboard/mouse mapping (`VICTORY_KBM_GTA=1`).
- **`victory-gpu`**: Translates D3D12.X command lists, descriptors, unified memory tracking (card rehash & Pareto residency), and Xbox BCn texture detiling / DXIL shaders to standard Windows D3D12.

## Key Optimizations in This Fork

- **Clean Rendering & Detiling**: Preserves full BCn texture/skin detiling fixes, Wave32 handling, and `PreludeUpload` frame ordering.
- **NVIDIA RTX Throughput Tuning**:
  - Pareto memory residency regulation enabled by default to prevent WDDM/PCIe paging stalls.
  - Batched GPU card rehashes (`VICTORY_CARD_REHASH_RUNS=1`) and multi-part command list submission (`VICTORY_THIN=1`, `VICTORY_JOIN_PARTS=8`).
  - Blocking fence waits via `WaitOnAddress` / `WakeByAddressAll` instead of `E_NOTIMPL` spin fallbacks.
  - Automatic exclusion of unused ray-tracing resolve/probe compute pipelines when `VICTORY_BVH_BUILDER` is disabled.
  - Experimental `VICTORY_PORT_*` renderer ports disabled by default for NVIDIA stability.

## Build & Run

### Prerequisites
- Windows 10/11 (x64) with DirectX 12 capable GPU (tested on NVIDIA RTX A4500)
- Rust toolchain (MSVC) & Visual Studio Build Tools

### Build
```powershell
cargo build --release --locked -p victory
