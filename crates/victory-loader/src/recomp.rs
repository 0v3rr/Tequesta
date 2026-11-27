//! Static PE32+ recompilation analysis, function lifting, ahead-of-time
//! console-instruction rewriting, and import-trampoline generation.
//!
//! Works purely on PE32+ bytes so it runs and tests cross-platform without
//! loading the image into the Windows loader.

use victory_pe::layout::{self, dir};
use victory_pe::{ImageInfo, ImportSymbol};

use crate::console_insn_decode::{ConsoleOnly, decode};
use crate::{PrepareError, PreparedImage, prepare};

const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const DIR64: u16 = 10;

/// A function range lifted from the PE32+ `.pdata` exception directory (or
/// synthesized from entry/export RVAs when `.pdata` is absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionBlock {
    pub begin_rva: u32,
    pub end_rva: u32,
    pub unwind_rva: u32,
    pub is_entry: bool,
}

/// Zen 2 console-only instructions identified for static ahead-of-time handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleRewriteKind {
    Monitorx,
    Mwaitx,
    Clwb,
    Clzero,
    ClzeroLoop,
}

/// A console-only instruction site inside an executable section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleRewriteSite {
    pub rva: u32,
    pub file_offset: usize,
    pub len: usize,
    pub kind: ConsoleRewriteKind,
    /// Fixed-length in-place replacement bytes (`None` when trampoline-only).
    pub patch: Option<Vec<u8>>,
}

/// Planned import trampoline slot for an IAT entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportTrampoline {
    pub index: usize,
    pub dll: String,
    pub symbol: String,
    pub iat_slot_rva: u32,
}

/// `IMAGE_REL_BASED_DIR64` relocation record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelocEntry {
    pub rva: u32,
    pub file_offset: usize,
}

/// Complete static recompilation plan extracted from a PE32+ image.
#[derive(Debug, Clone)]
pub struct RecompPlan {
    pub image_base: u64,
    pub size_of_image: u32,
    pub entry_rva: u32,
    pub functions: Vec<FunctionBlock>,
    pub rewrite_sites: Vec<ConsoleRewriteSite>,
    pub import_trampolines: Vec<ImportTrampoline>,
    pub relocations: Vec<RelocEntry>,
}

/// Summary counts of applying static patches to a binary buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PatchStats {
    pub patched_sites: usize,
    pub patched_bytes: usize,
    pub deferred_sites: usize,
}

impl RecompPlan {
    /// Human-readable summary of lifted functions, rewrite sites, imports, and relocations.
    pub fn format_summary(&self) -> String {
        let patchable = self
            .rewrite_sites
            .iter()
            .filter(|s| s.patch.is_some())
            .count();
        let deferred = self.rewrite_sites.len().saturating_sub(patchable);
        format!(
            "image_base={:#x} size={:#x} entry={:#x}\n\
             functions={} (pdata/entries) relocations={} import_trampolines={}\n\
             console_sites={} (static_patchable={}, trampoline_deferred={})",
            self.image_base,
            self.size_of_image,
            self.entry_rva,
            self.functions.len(),
            self.relocations.len(),
            self.import_trampolines.len(),
            self.rewrite_sites.len(),
            patchable,
            deferred,
        )
    }
}

/// Emits a 32-byte x86-64 import/unresolved-site trampoline:
/// `mov rcx, site_addr; mov rax, target_addr; jmp rax; int3...`
pub fn emit_import_trampoline(site_addr: u64, target_addr: u64) -> [u8; 32] {
    let mut stub = [0xCCu8; 32];
    stub[0..2].copy_from_slice(&[0x48, 0xB9]);
    stub[2..10].copy_from_slice(&site_addr.to_le_bytes());
    stub[10..12].copy_from_slice(&[0x48, 0xB8]);
    stub[12..20].copy_from_slice(&target_addr.to_le_bytes());
    stub[20..22].copy_from_slice(&[0xFF, 0xE0]);
    stub
}

/// Analyzes a PE32+ image and builds a [`RecompPlan`].
pub fn analyze(original: &[u8]) -> Result<RecompPlan, PrepareError> {
    let info = ImageInfo::parse(original)?;
    let functions = lift_functions(original, &info);
    let rewrite_sites = scan_console_sites(original, &info, &functions);
    let import_trampolines = plan_import_trampolines(&info);
    let relocations = extract_relocations(original, &info);
    Ok(RecompPlan {
        image_base: info.image_base,
        size_of_image: info.size_of_image,
        entry_rva: info.entry_rva,
        functions,
        rewrite_sites,
        import_trampolines,
        relocations,
    })
}

/// Applies all safe fixed-length static patches in `plan` to a copy of `original`.
pub fn apply_static_patches(original: &[u8], plan: &RecompPlan) -> (Vec<u8>, PatchStats) {
    let mut out = original.to_vec();
    let mut stats = PatchStats::default();
    for site in &plan.rewrite_sites {
        let Some(patch) = &site.patch else {
            stats.deferred_sites += 1;
            continue;
        };
        let end = site.file_offset.saturating_add(patch.len());
        if patch.len() == site.len && end <= out.len() {
            out[site.file_offset..end].copy_from_slice(patch);
            stats.patched_sites += 1;
            stats.patched_bytes += patch.len();
        } else {
            stats.deferred_sites += 1;
        }
    }
    (out, stats)
}

/// Runs [`prepare`] and applies static console-instruction patches in one pass.
pub fn prepare_recompiled(original: &[u8]) -> Result<(PreparedImage, RecompPlan, PatchStats), PrepareError> {
    let plan = analyze(original)?;
    let (patched, stats) = apply_static_patches(original, &plan);
    let prepared = prepare(&patched)?;
    Ok((prepared, plan, stats))
}

fn lift_functions(bytes: &[u8], info: &ImageInfo) -> Vec<FunctionBlock> {
    let mut out = Vec::new();
    if let Some(d) = layout::data_directory_offset(bytes, dir::EXCEPTION)
        && let (Some(rva), Some(size)) = (layout::read_u32(bytes, d), layout::read_u32(bytes, d + 4))
        && size >= 12
        && let Some(start) = info.rva_to_file_offset(rva)
    {
        let start = start as usize;
        let end = start.saturating_add(size as usize).min(bytes.len());
        let mut at = start;
        while at + 12 <= end {
            let begin = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            let end_rva = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap());
            let unwind = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap());
            if begin != 0 && end_rva > begin {
                out.push(FunctionBlock {
                    begin_rva: begin,
                    end_rva,
                    unwind_rva: unwind,
                    is_entry: begin == info.entry_rva,
                });
            }
            at += 12;
        }
    }
    if info.entry_rva != 0 && !out.iter().any(|f| f.begin_rva == info.entry_rva) {
        out.push(FunctionBlock {
            begin_rva: info.entry_rva,
            end_rva: info.entry_rva,
            unwind_rva: 0,
            is_entry: true,
        });
    }
    for exp in &info.exports {
        if let Some(rva) = exp.rva
            && rva != 0
            && !out.iter().any(|f| f.begin_rva == rva)
        {
            out.push(FunctionBlock {
                begin_rva: rva,
                end_rva: rva,
                unwind_rva: 0,
                is_entry: true,
            });
        }
    }
    out.sort_by_key(|f| (f.begin_rva, f.end_rva));
    out.dedup_by(|a, b| a.begin_rva == b.begin_rva && a.end_rva == b.end_rva);
    out
}

fn scan_console_sites(
    bytes: &[u8],
    info: &ImageInfo,
    functions: &[FunctionBlock],
) -> Vec<ConsoleRewriteSite> {
    let mut sites = Vec::new();
    let pdata_ranges: Vec<(u32, u32)> = functions
        .iter()
        .filter(|f| f.end_rva > f.begin_rva)
        .map(|f| (f.begin_rva, f.end_rva))
        .collect();

    for sec in &info.sections {
        if sec.characteristics & IMAGE_SCN_MEM_EXECUTE == 0 || sec.file_size == 0 {
            continue;
        }
        let sec_start = sec.file_offset as usize;
        let sec_end = sec_start.saturating_add(sec.file_size as usize).min(bytes.len());
        if sec_start >= sec_end {
            continue;
        }
        let sec_rva_end = sec.rva.saturating_add((sec_end - sec_start) as u32);
        let subranges: Vec<(u32, u32)> = if pdata_ranges.is_empty() {
            vec![(sec.rva, sec_rva_end)]
        } else {
            pdata_ranges
                .iter()
                .filter_map(|&(b, e)| {
                    let lo = b.max(sec.rva);
                    let hi = e.min(sec_rva_end);
                    (hi > lo).then_some((lo, hi))
                })
                .collect()
        };

        for (rva_lo, rva_hi) in subranges {
            let mut rva = rva_lo;
            while rva < rva_hi {
                let off = sec_start + (rva - sec.rva) as usize;
                let max_len = (rva_hi - rva) as usize;
                let slice = &bytes[off..(off + max_len).min(sec_end)];
                if let Some((what, len)) = decode(slice) {
                    let (kind, patch) = match what {
                        ConsoleOnly::Monitorx => (ConsoleRewriteKind::Monitorx, Some(vec![0x0F, 0x1F, 0x00])),
                        ConsoleOnly::Mwaitx => (ConsoleRewriteKind::Mwaitx, Some(vec![0xF3, 0x90, 0x90])),
                        ConsoleOnly::Clwb => (ConsoleRewriteKind::Clwb, Some(nop_sled(len))),
                        ConsoleOnly::Clzero => (ConsoleRewriteKind::Clzero, None),
                        ConsoleOnly::ClzeroLoop { .. } => (ConsoleRewriteKind::ClzeroLoop, None),
                        _ => {
                            rva += 1;
                            continue;
                        }
                    };
                    if len > 0 && len <= max_len {
                        sites.push(ConsoleRewriteSite {
                            rva,
                            file_offset: off,
                            len,
                            kind,
                            patch,
                        });
                        rva += len as u32;
                        continue;
                    }
                }
                rva += 1;
            }
        }
    }
    sites
}

fn nop_sled(len: usize) -> Vec<u8> {
    match len {
        0 => Vec::new(),
        1 => vec![0x90],
        2 => vec![0x66, 0x90],
        3 => vec![0x0F, 0x1F, 0x00],
        4 => vec![0x0F, 0x1F, 0x40, 0x00],
        5 => vec![0x0F, 0x1F, 0x44, 0x00, 0x00],
        6 => vec![0x66, 0x0F, 0x1F, 0x44, 0x00, 0x00],
        7 => vec![0x0F, 0x1F, 0x80, 0x00, 0x00, 0x00, 0x00],
        8 => vec![0x0F, 0x1F, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
        n => vec![0x90; n],
    }
}

fn plan_import_trampolines(info: &ImageInfo) -> Vec<ImportTrampoline> {
    let mut out = Vec::new();
    for module in info.imports.iter().chain(&info.delay_imports) {
        for entry in &module.entries {
            let symbol = match &entry.symbol {
                ImportSymbol::Name { name, .. } => name.clone(),
                ImportSymbol::Ordinal(o) => format!("#{o}"),
            };
            out.push(ImportTrampoline {
                index: out.len(),
                dll: module.dll.clone(),
                symbol,
                iat_slot_rva: entry.iat_slot_rva,
            });
        }
    }
    out
}

fn extract_relocations(bytes: &[u8], info: &ImageInfo) -> Vec<RelocEntry> {
    let mut out = Vec::new();
    let Some(d) = layout::data_directory_offset(bytes, dir::BASERELOC) else {
        return out;
    };
    let (Some(rva), Some(size)) = (layout::read_u32(bytes, d), layout::read_u32(bytes, d + 4)) else {
        return out;
    };
    let Some(start) = info.rva_to_file_offset(rva).filter(|_| size != 0) else {
        return out;
    };
    let (mut at, end) = (start as usize, (start as usize).saturating_add(size as usize).min(bytes.len()));
    while at + 8 <= end {
        let (Some(page), Some(block)) = (layout::read_u32(bytes, at), layout::read_u32(bytes, at + 4)) else {
            break;
        };
        let block = block as usize;
        if block < 8 || at + block > end {
            break;
        }
        for entry in (at + 8..at + block).step_by(2) {
            let e = u16::from_le_bytes([bytes[entry], bytes[entry + 1]]);
            if e >> 12 == DIR64 {
                let target_rva = page + u32::from(e & 0xFFF);
                if let Some(file_offset) = info.rva_to_file_offset(target_rva) {
                    out.push(RelocEntry {
                        rva: target_rva,
                        file_offset: file_offset as usize,
                    });
                }
            }
        }
        at += block;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_synthetic_pe(code: &[u8]) -> Vec<u8> {
        // Minimal valid PE32+ with .text (0x1000), .pdata (0x2000), .reloc (0x3000), .idata (0x4000).
        let mut pe = vec![0u8; 0x1000];
        let put16 = |b: &mut [u8], o: usize, v: u16| b[o..o + 2].copy_from_slice(&v.to_le_bytes());
        let put32 = |b: &mut [u8], o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        let put64 = |b: &mut [u8], o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());

        // DOS header
        pe[0..2].copy_from_slice(b"MZ");
        put32(&mut pe, 0x3C, 0x80);

        // NT headers at 0x80
        let nt = 0x80usize;
        pe[nt..nt + 4].copy_from_slice(b"PE\0\0");
        // File header (20 bytes)
        put16(&mut pe, nt + 4, 0x8664); // AMD64
        put16(&mut pe, nt + 6, 4); // 4 sections
        put16(&mut pe, nt + 20, 240); // SizeOfOptionalHeader
        put16(&mut pe, nt + 22, 0x0022); // EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE

        // Optional header PE32+ at nt + 24
        let opt = nt + 24;
        put16(&mut pe, opt, 0x20B);
        put32(&mut pe, opt + 16, 0x1000); // AddressOfEntryPoint
        put64(&mut pe, opt + 24, 0x1_4000_0000); // ImageBase
        put32(&mut pe, opt + 32, 0x1000); // SectionAlignment
        put32(&mut pe, opt + 36, 0x200); // FileAlignment
        put16(&mut pe, opt + 48, 6);
        put16(&mut pe, opt + 68, 3); // CONSOLE
        put32(&mut pe, opt + 56, 0x5000); // SizeOfImage
        put32(&mut pe, opt + 60, 0x400); // SizeOfHeaders
        put32(&mut pe, opt + 108, 16); // NumberOfRvaAndSizes

        let dd = opt + 112;
        // IMPORT (1)
        put32(&mut pe, dd + 8, 0x4000);
        put32(&mut pe, dd + 12, 40);
        // EXCEPTION (3)
        put32(&mut pe, dd + 3 * 8, 0x2000);
        put32(&mut pe, dd + 3 * 8 + 4, 12);
        // BASERELOC (5)
        put32(&mut pe, dd + 5 * 8, 0x3000);
        put32(&mut pe, dd + 5 * 8 + 4, 12);

        let sec_hdr = opt + 240;
        let write_sec = |b: &mut [u8], idx: usize, name: &[u8; 8], rva: u32, raw: u32, chars: u32| {
            let s = sec_hdr + idx * 40;
            b[s..s + 8].copy_from_slice(name);
            put32(b, s + 8, 0x200);
            put32(b, s + 12, rva);
            put32(b, s + 16, 0x200);
            put32(b, s + 20, raw);
            put32(b, s + 36, chars);
        };
        write_sec(&mut pe, 0, b".text\0\0\0", 0x1000, 0x400, 0x6000_0020);
        write_sec(&mut pe, 1, b".pdata\0\0", 0x2000, 0x600, 0x4000_0040);
        write_sec(&mut pe, 2, b".reloc\0\0", 0x3000, 0x800, 0x4200_0040);
        write_sec(&mut pe, 3, b".idata\0\0", 0x4000, 0xA00, 0xC000_0040);

        // .text at 0x400
        pe[0x400..0x400 + code.len()].copy_from_slice(code);
        // .pdata at 0x600: single RUNTIME_FUNCTION covering 0x1000..0x1000+code.len()
        put32(&mut pe, 0x600, 0x1000);
        put32(&mut pe, 0x604, 0x1000 + code.len() as u32);
        put32(&mut pe, 0x608, 0x2020);

        // .reloc at 0x800: page 0x1000, block size 12, DIR64 at +0x10
        put32(&mut pe, 0x800, 0x1000);
        put32(&mut pe, 0x804, 12);
        put16(&mut pe, 0x808, (DIR64 << 12) | 0x0010);

        // .idata at 0xA00: 1 import descriptor (kernel32.dll -> ExitProcess) + null terminator
        put32(&mut pe, 0xA00, 0x4040); // OriginalFirstThunk (ILT)
        put32(&mut pe, 0xA0C, 0x4060); // Name RVA
        put32(&mut pe, 0xA10, 0x4050); // FirstThunk (IAT)
        put64(&mut pe, 0xA40, 0x4070); // ILT[0] -> Hint/Name
        put64(&mut pe, 0xA50, 0x4070); // IAT[0]
        pe[0xA60..0xA60 + 13].copy_from_slice(b"kernel32.dll\0");
        pe[0xA72..0xA72 + 12].copy_from_slice(b"ExitProcess\0");

        pe
    }

    #[test]
    fn analyzes_lifts_and_patches_synthetic_pe32_image() {
        let mut code = Vec::new();
        code.extend_from_slice(&[0x0F, 0x01, 0xFA]); // monitorx (3 bytes)
        code.extend_from_slice(&[0x0F, 0x01, 0xFB]); // mwaitx (3 bytes)
        code.extend_from_slice(&[0x66, 0x0F, 0xAE, 0x30]); // clwb [rax] (4 bytes)
        // clzero loop: clzero; add rax, 0x40; cmp rax, rbx; jb -12
        code.extend_from_slice(&[
            0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40, 0x48, 0x39, 0xD8, 0x72, 0xF4,
        ]);
        let tail_offset = code.len();
        code.push(0xC3); // ret

        let pe = build_synthetic_pe(&code);
        let plan = analyze(&pe).expect("synthetic PE parses");
        assert_eq!(plan.functions.len(), 1);
        assert_eq!(plan.functions[0].begin_rva, 0x1000);
        assert!(plan.functions[0].is_entry);
        assert_eq!(plan.relocations.len(), 1);
        assert_eq!(plan.relocations[0].rva, 0x1010);
        assert_eq!(plan.import_trampolines.len(), 1);
        assert_eq!(plan.import_trampolines[0].dll, "kernel32.dll");
        assert_eq!(plan.import_trampolines[0].symbol, "ExitProcess");

        let kinds: Vec<_> = plan.rewrite_sites.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [
                ConsoleRewriteKind::Monitorx,
                ConsoleRewriteKind::Mwaitx,
                ConsoleRewriteKind::Clwb,
                ConsoleRewriteKind::ClzeroLoop,
            ]
        );

        let (patched, stats) = apply_static_patches(&pe, &plan);
        assert_eq!(stats.patched_sites, 3);
        assert_eq!(stats.patched_bytes, 10);
        assert_eq!(stats.deferred_sites, 1);
        assert_eq!(&patched[0x400..0x403], &[0x0F, 0x1F, 0x00]); // monitorx -> nop3
        assert_eq!(&patched[0x403..0x406], &[0xF3, 0x90, 0x90]); // mwaitx -> pause; nop2
        assert_eq!(&patched[0x406..0x40A], &[0x0F, 0x1F, 0x40, 0x00]); // clwb -> nop4
        assert_eq!(patched[0x400 + tail_offset], 0xC3); // untouched ret preserved
    }

    #[test]
    fn emits_valid_x64_import_trampoline() {
        let stub = emit_import_trampoline(0x1122_3344_5566_7788, 0x8877_6655_4433_2211);
        assert_eq!(&stub[0..2], &[0x48, 0xB9]);
        assert_eq!(&stub[2..10], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&stub[10..12], &[0x48, 0xB8]);
        assert_eq!(&stub[12..20], &0x8877_6655_4433_2211u64.to_le_bytes());
        assert_eq!(&stub[20..22], &[0xFF, 0xE0]);
        assert!(stub[22..].iter().all(|&b| b == 0xCC));
    }
}
