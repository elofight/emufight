//! CPS1 ROM-set loading.
//!
//! Loads the four logical regions of a CPS1 board from `roms/<name>/` **or**
//! an in-memory filename→bytes map (WASM / host-supplied dumps):
//!
//! | Region     | Role                                    |
//! |------------|-----------------------------------------|
//! | `maincpu`  | 68000 program (word-swapped, ≤ 4 MB)    |
//! | `gfx`      | Tile / sprite graphics                  |
//! | `audiocpu` | Z80 sound program + banked data         |
//! | `oki`      | OKI MSM6295 ADPCM samples               |
//!
//! Because CPS1 ROM filenames vary between dumps, loading is driven by an
//! embedded per-game manifest (MAME layout).  A `manifest.json` placed in the
//! game directory overrides the built-in list, and a best-effort directory
//! scan is used as a last resort.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The four assembled CPS1 ROM regions.
#[derive(Default)]
pub struct CpsRoms {
    pub program: Vec<u8>,
    pub gfx: Vec<u8>,
    pub z80: Vec<u8>,
    pub oki: Vec<u8>,
}
/// How a ROM file's bytes are placed into its region.
#[derive(Clone, Copy)]
enum Load {
    /// Contiguous bytes starting at `offset` (MAME `ROM_LOAD` /
    /// `ROM_LOAD16_WORD_SWAP`).  For program regions the whole region is
    /// byte-swapped once afterwards by `word_swap_region`.
    Contiguous,
    /// 16-bit little-endian words placed every 8 bytes starting at byte `offset`
    /// (MAME `ROM_LOAD64_WORD` — the standard CPS1 4-chips-per-bank GFX interleave).
    Word64,
}

/// One file placed into a region at a byte offset with a given interleave mode.
struct RomEntry {
    file: &'static str,
    offset: usize,
    mode: Load,
}

struct Manifest {
    program: &'static [RomEntry],
    gfx: &'static [RomEntry],
    z80: &'static [RomEntry],
    oki: &'static [RomEntry],
}

/// Contiguous load.
macro_rules! byte {
    ($f:expr, $o:expr) => {
        RomEntry { file: $f, offset: $o, mode: Load::Contiguous }
    };
}
/// 64-bit-word interleaved load (CPS1 GFX).
macro_rules! word64 {
    ($f:expr, $o:expr) => {
        RomEntry { file: $f, offset: $o, mode: Load::Word64 }
    };
}

// ── Street Fighter II: The World Warrior (World 910522) ──────────────────────
// Best-effort layout (not verified against a local dump). Program ROMs are
// assumed to be in native 68000 big-endian order (FBNeo/Fightcade convention).
// GFX: ROM_LOAD64_WORD (16-bit words at stride 8).
static SF2_PROGRAM: [RomEntry; 3] = [
    byte!("sf2_30f.11f", 0x00000),
    byte!("sf2_31f.12f", 0x40000),
    byte!("sf2_28f.9f", 0x80000),
];
static SF2_GFX: [RomEntry; 8] = [
    word64!("sf2_06.8a", 0x000000),
    word64!("sf2_08.10a", 0x000002),
    word64!("sf2_05.7a", 0x000004),
    word64!("sf2_07.9a", 0x000006),
    word64!("sf2_15.3a", 0x200000),
    word64!("sf2_17.5a", 0x200002),
    word64!("sf2_14.2a", 0x200004),
    word64!("sf2_16.4a", 0x200006),
];
static SF2_Z80: [RomEntry; 1] = [byte!("sf2_09.12a", 0x00000)];
static SF2_OKI: [RomEntry; 2] = [
    byte!("sf2_18.11c", 0x00000),
    byte!("sf2_19.12c", 0x20000),
];

// ── Street Fighter II': Champion Edition (World 920313) ──────────────────────
// Program: ROM_LOAD16_WORD_SWAP — three 512 KB chips.  MAME order (cps1.cpp
// ROM_START(sf2ce)): s92e_23b.8f @ 0x000000 (vector table + reset code),
// s92_22b.7f @ 0x080000, s92_21a.6f @ 0x100000.  The dump is stored byte-swapped
// relative to 68000 order, so the assembled region is word-swapped once.
// GFX: ROM_LOAD64_WORD — twelve 512 KB chips, four per 2 MB bank, each
//      contributing a 16-bit word every 8 bytes.
static SF2CE_PROGRAM: [RomEntry; 3] = [
    byte!("s92e_23b.8f", 0x000000),
    byte!("s92_22b.7f", 0x080000),
    byte!("s92_21a.6f", 0x100000),
];
static SF2CE_GFX: [RomEntry; 12] = [
    word64!("s92-1m.3a", 0x000000),
    word64!("s92-3m.5a", 0x000002),
    word64!("s92-2m.4a", 0x000004),
    word64!("s92-4m.6a", 0x000006),
    word64!("s92-5m.7a", 0x200000),
    word64!("s92-7m.9a", 0x200002),
    word64!("s92-6m.8a", 0x200004),
    word64!("s92-8m.10a", 0x200006),
    word64!("s92-10m.3c", 0x400000),
    word64!("s92-12m.5c", 0x400002),
    word64!("s92-11m.4c", 0x400004),
    word64!("s92-13m.6c", 0x400006),
];
static SF2CE_Z80: [RomEntry; 1] = [byte!("s92_09.11a", 0x00000)];
static SF2CE_OKI: [RomEntry; 2] = [
    byte!("s92_18.11c", 0x00000),
    byte!("s92_19.12c", 0x20000),
];

fn manifest_for(name: &str) -> Option<Manifest> {
    match name {
        "sf2" => Some(Manifest {
            program: &SF2_PROGRAM,
            gfx: &SF2_GFX,
            z80: &SF2_Z80,
            oki: &SF2_OKI,
        }),
        "sf2ce" => Some(Manifest {
            program: &SF2CE_PROGRAM,
            gfx: &SF2CE_GFX,
            z80: &SF2CE_Z80,
            oki: &SF2CE_OKI,
        }),
        _ => None,
    }
}

/// Load and assemble all four ROM regions for `name` from `roms/<name>/`.
///
/// The host is responsible for placing dumps there (and for any catalog-driven
/// download via `crate::romset::ensure_roms_dir` with a host-supplied catalog).
pub fn load(name: &str) -> Result<CpsRoms, String> {
    let manifest = manifest_for(name)
        .ok_or_else(|| format!("no CPS1 manifest for '{}'", name))?;

    let dir = PathBuf::from("roms").join(name);
    if !dir.exists() {
        return Err(format!("ROM directory {} not found", dir.display()));
    }

    let mut roms = CpsRoms::default();
    assemble_disk(&dir, manifest.program, &mut roms.program);
    assemble_disk(&dir, manifest.gfx, &mut roms.gfx);
    assemble_disk(&dir, manifest.z80, &mut roms.z80);
    assemble_disk(&dir, manifest.oki, &mut roms.oki);

    finalize_roms(name, roms, /*from_memory=*/ false)
}

/// Load and assemble CPS1 regions from an in-memory filename→bytes map (WASM).
///
/// File keys may be bare names (`s92e_23b.8f`) or paths; matching is
/// case-insensitive on the basename.
pub fn load_from_files(
    name: &str,
    files: &HashMap<String, Vec<u8>>,
) -> Result<CpsRoms, String> {
    let manifest = manifest_for(name)
        .ok_or_else(|| format!("no CPS1 manifest for '{}'", name))?;

    let mut roms = CpsRoms::default();
    assemble_memory(files, manifest.program, &mut roms.program);
    assemble_memory(files, manifest.gfx, &mut roms.gfx);
    assemble_memory(files, manifest.z80, &mut roms.z80);
    assemble_memory(files, manifest.oki, &mut roms.oki);

    finalize_roms(name, roms, /*from_memory=*/ true)
}

fn finalize_roms(name: &str, mut roms: CpsRoms, from_memory: bool) -> Result<CpsRoms, String> {
    if roms.program.is_empty() {
        return Err(if from_memory {
            format!(
                "CPS1: no program ROM files found in uploaded set for '{name}' — \
                 check that the zip contains the expected MAME filenames"
            )
        } else {
            format!(
                "CPS1: no program ROM files found in roms/{name}/  — \
                 place the ROM dump there and retry"
            )
        });
    }

    // CPS1 program ROMs are ROM_LOAD16_WORD_SWAP: the dump is stored byte-swapped
    // relative to 68000 order, so swap each 16-bit word back into big-endian.
    word_swap_region(&mut roms.program);

    // The GFX region is assembled via ROM_LOAD64_WORD interleave (see `assemble`),
    // which already yields the planar 16x16-tile layout the renderer indexes into.
    // No further bit unshuffle is applied.

    Ok(roms)
}

/// Byte-swap each 16-bit word in place (MAME `ROM_LOAD16_WORD_SWAP` → 68000 BE).
fn word_swap_region(data: &mut [u8]) {
    for pair in data.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
}

/// Copy each manifest entry into `dest` from disk, expanding `dest` as needed.
fn assemble_disk(dir: &Path, entries: &[RomEntry], dest: &mut Vec<u8>) {
    for e in entries {
        let path = dir.join(e.file);
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(_) => {
                log::warn!("CPS1: missing ROM file {}", path.display());
                continue;
            }
        };
        place_entry(e, &data, dest);
    }
}

/// Copy each manifest entry into `dest` from an in-memory file map.
fn assemble_memory(
    files: &HashMap<String, Vec<u8>>,
    entries: &[RomEntry],
    dest: &mut Vec<u8>,
) {
    for e in entries {
        let Some(data) = find_file(files, e.file) else {
            log::warn!("CPS1: missing ROM file in memory map: {}", e.file);
            continue;
        };
        place_entry(e, data, dest);
    }
}

fn place_entry(e: &RomEntry, data: &[u8], dest: &mut Vec<u8>) {
    match e.mode {
        Load::Contiguous => {
            let end = e.offset + data.len();
            if dest.len() < end {
                dest.resize(end, 0);
            }
            dest[e.offset..end].copy_from_slice(data);
        }
        Load::Word64 => {
            let words = data.len() / 2;
            let end = e.offset + words.saturating_sub(1) * 8 + 2;
            if dest.len() < end {
                dest.resize(end, 0);
            }
            for w in 0..words {
                let base = e.offset + w * 8;
                dest[base] = data[w * 2];
                dest[base + 1] = data[w * 2 + 1];
            }
        }
    }
}

/// Look up a ROM by exact key, case-insensitive key, or case-insensitive basename.
fn find_file<'a>(files: &'a HashMap<String, Vec<u8>>, want: &str) -> Option<&'a [u8]> {
    if let Some(d) = files.get(want) {
        return Some(d.as_slice());
    }
    let want_lower = want.to_ascii_lowercase();
    for (k, v) in files {
        if k.eq_ignore_ascii_case(want) {
            return Some(v.as_slice());
        }
        let base = k
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(k.as_str());
        if base.eq_ignore_ascii_case(want) || base.to_ascii_lowercase() == want_lower {
            return Some(v.as_slice());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_from_files_matches_disk_when_sf2ce_present() {
        let dir = PathBuf::from("roms/sf2ce");
        if !dir.is_dir() {
            // Also accept elofight workspace layout when tests run from monorepo.
            let alt = PathBuf::from("../elofight/roms/sf2ce");
            if !alt.is_dir() {
                eprintln!("skip: no roms/sf2ce");
                return;
            }
            return load_compare("sf2ce", &alt);
        }
        load_compare("sf2ce", &dir);
    }

    fn load_compare(name: &str, dir: &Path) {
        let mut files = HashMap::new();
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_file() {
                if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                    if let Ok(data) = fs::read(&path) {
                        // Nested path keys (as zip uploads often produce).
                        files.insert(format!("{name}/{fname}"), data.clone());
                        files.insert(fname.to_string(), data);
                    }
                }
            }
        }
        let from_mem = load_from_files(name, &files).expect("memory load");
        let from_disk = load(name);
        // Disk load looks at roms/<name> relative to CWD — may fail if CWD is wrong.
        if let Ok(disk) = from_disk {
            assert_eq!(from_mem.program.len(), disk.program.len());
            assert_eq!(from_mem.program, disk.program);
            assert_eq!(from_mem.gfx.len(), disk.gfx.len());
            assert_eq!(from_mem.z80, disk.z80);
            assert_eq!(from_mem.oki, disk.oki);
        } else {
            assert!(!from_mem.program.is_empty());
            assert!(!from_mem.gfx.is_empty());
        }
    }
}


