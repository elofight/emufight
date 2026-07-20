//! ROM-set manager: resolves dependencies, downloads ZIPs, extracts them,
//! and loads the resulting files into the `SystemBus`.
//!
//! # Lookup format (roms.json)
//! ```json
//! {
//!   "kof97": {
//!     "download": "https://example.com/kof97.zip",
//!     "require": ["neogeo"]
//!   },
//!   "neogeo": {
//!     "download": "https://example.com/neogeo.zip"
//!   }
//! }
//! ```
//!
//! # On-disk cache
//! Each ROM set is extracted into `roms/<name>/`.  A `.done` sentinel file
//! prevents redundant downloads on subsequent runs.
//!
//! # NeoGeo file identification (FBNeo naming conventions)
//! | Extension(s)          | Role                                |
//! |-----------------------|-------------------------------------|
//! | `.sp1`                | BIOS (from `neogeo` dependency)     |
//! | `sfix.sfix`           | System fix-layer S-ROM              |
//! | `sm1.sm1`             | System Z80 M1 ROM                   |
//! | `.m1`                 | Game Z80 M1 ROM                     |
//! | `.p1`, `.p2`, …       | P-ROM (68k program, concatenated)   |
//! | `.sp2`                | P-ROM high bank (alternative name)  |
//! | `.s1`                 | Game fix-layer S-ROM                |
//! | `.c1`+`.c2`, `.c3`+`.c4`, … | Sprite C-ROMs (byte-interleaved) |

#[cfg(feature = "native-romset")]
use std::collections::HashSet;
use std::fs;
#[cfg(feature = "native-romset")]
use std::fs::File;
#[cfg(feature = "native-romset")]
use std::io;
use std::path::{Path, PathBuf};


#[cfg(feature = "native-romset")]
use crate::catalog::RomCatalog;
use crate::neogeo::bus::SystemBus;
use crate::neogeo::cart;

/// Download (if needed) and load a named ROM set into the bus.
///
/// **Host responsibility:** catalog `download` URLs, licensing, and cache
/// layout.  This is an optional convenience (`native-romset`); production
/// hosts often stage dumps themselves and call [`load_prepared_game`] only.
///
/// System BIOS/sfix/sm1/lo load from host disk via
/// [`SystemBus::load_host_system_roms`](crate::neogeo::bus::SystemBus::load_host_system_roms).
#[cfg(feature = "native-romset")]
pub fn prepare_and_load(
    bus: &mut SystemBus,
    name: &str,
    catalog: &RomCatalog,
) -> Result<(), String> {
    let entries = catalog
        .entries()
        .ok_or("ROM catalog root must be a JSON object")?;

    if !entries.contains_key(name) {
        return Err(format!("'{name}' not found in ROM catalog (check spelling)"));
    }

    let load_order = collect_deps(name, entries);
    log::info!("ROM load order: {:?}", load_order);

    let roms_base = PathBuf::from("roms");
    fs::create_dir_all(&roms_base)
        .map_err(|e| format!("cannot create roms/ directory: {}", e))?;

    for set_name in &load_order {
        let entry = entries
            .get(set_name.as_str())
            .ok_or_else(|| format!("dependency '{set_name}' not found in ROM catalog"))?;

        let set_dir = roms_base.join(set_name);
        fs::create_dir_all(&set_dir)
            .map_err(|e| format!("cannot create {}: {}", set_dir.display(), e))?;

        let done_marker = set_dir.join(".done");
        if done_marker.exists() {
            log::info!("{} already cached in {}", set_name, set_dir.display());
        } else if let Some(url) = entry.get("download").and_then(|v| v.as_str()) {
            log::info!("Downloading {} ...", set_name);
            download_and_extract(url, &set_dir)
                .map_err(|e| format!("failed to get '{}': {}", set_name, e))?;
            fs::write(&done_marker, "")
                .map_err(|e| format!("cannot write .done marker: {}", e))?;
            log::info!("{} extracted to {}", set_name, set_dir.display());
        }
        // Platform-only entries (no download) must already exist on disk.
    }

    load_into_bus(bus, &roms_base, &load_order, name)
}

pub fn load_prepared_game(bus: &mut SystemBus, name: &str) -> Result<(), String> {
    let roms_base = PathBuf::from("roms");
    load_into_bus(bus, &roms_base, &[name.to_string()], name)
}

/// Download and extract the ROM ZIP for `name` (and dependencies) into
/// `roms/<name>/` using the host-supplied catalog.  No-op when `.done` exists.
///
/// **Optional convenience only** (`native-romset`).  The host owns URL
/// selection, licensing, and whether downloads are allowed at all.
#[cfg(feature = "native-romset")]
pub fn ensure_roms_dir(name: &str, catalog: &RomCatalog) -> Result<(), String> {
    let entries = catalog
        .entries()
        .ok_or("ROM catalog root must be a JSON object")?;

    if !entries.contains_key(name) {
        return Err(format!("'{name}' not found in ROM catalog"));
    }

    let load_order = collect_deps(name, entries);
    let roms_base = PathBuf::from("roms");
    fs::create_dir_all(&roms_base).map_err(|e| format!("cannot create roms/: {e}"))?;

    for set_name in &load_order {
        let entry = entries
            .get(set_name.as_str())
            .ok_or_else(|| format!("dependency '{set_name}' not found in ROM catalog"))?;

        let Some(url) = entry.get("download").and_then(|v| v.as_str()) else {
            continue; // platform-only / pre-seeded on disk
        };

        let set_dir = roms_base.join(set_name);
        fs::create_dir_all(&set_dir)
            .map_err(|e| format!("cannot create {}: {}", set_dir.display(), e))?;

        let done_marker = set_dir.join(".done");
        if !done_marker.exists() {
            log::info!("Downloading {} ...", set_name);
            download_and_extract(url, &set_dir)
                .map_err(|e| format!("failed to get '{set_name}': {e}"))?;
            fs::write(&done_marker, "").map_err(|e| format!("cannot write .done marker: {e}"))?;
            log::info!("{} extracted to {}", set_name, set_dir.display());
        }
    }
    Ok(())
}

// ── Dependency resolution ─────────────────────────────────────────────────────

/// Collect all transitive dependencies in topological order (deps first).
#[cfg(feature = "native-romset")]
fn collect_deps(
    name:    &str,
    entries: &serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    let mut order   = Vec::new();
    let mut visited = HashSet::new();
    dfs(name, entries, &mut visited, &mut order);
    order
}

#[cfg(feature = "native-romset")]
fn dfs(
    name:    &str,
    entries: &serde_json::Map<String, serde_json::Value>,
    visited: &mut HashSet<String>,
    order:   &mut Vec<String>,
) {
    if visited.contains(name) { return; }
    visited.insert(name.to_string());
    if let Some(entry) = entries.get(name) {
        if let Some(reqs) = entry.get("require").and_then(|v| v.as_array()) {
            for req in reqs {
                if let Some(req_name) = req.as_str() {
                    dfs(req_name, entries, visited, order);
                }
            }
        }
    }
    order.push(name.to_string());
}

// ── Download and extraction ───────────────────────────────────────────────────

#[cfg(feature = "native-romset")]
fn download_and_extract(url: &str, dest: &Path) -> Result<(), String> {
    // Download via ureq (pure Rust, no shell, portable). Host-supplied URL.
    let zip_path = dest.join("_download.zip");
    log::info!("Downloading: {}", url);
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("download failed for '{}': {}", url, e))?;
    {
        let mut reader = response.into_reader();
        let mut out_file = File::create(&zip_path)
            .map_err(|e| format!("cannot create download buffer '{}': {}", zip_path.display(), e))?;
        io::copy(&mut reader, &mut out_file)
            .map_err(|e| format!("download write error for '{}': {}", url, e))?;
    }

    // Extract all files, discarding any directory structure inside the zip.
    {
        let zip_file = File::open(&zip_path)
            .map_err(|e| format!("cannot open downloaded zip: {}", e))?;
        let mut archive = zip::ZipArchive::new(zip_file)
            .map_err(|e| format!("invalid zip archive: {}", e))?;

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)
                .map_err(|e| format!("zip entry error at index {}: {}", i, e))?;
            if entry.is_dir() { continue; }

            // Keep only the bare filename to avoid path-traversal issues.
            let file_name = Path::new(entry.name())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if file_name.is_empty() || file_name.starts_with('.') { continue; }

            let out_path = dest.join(&file_name);
            let mut out_file = File::create(&out_path)
                .map_err(|e| format!("cannot extract '{}': {}", file_name, e))?;
            io::copy(&mut entry, &mut out_file)
                .map_err(|e| format!("extraction error for '{}': {}", file_name, e))?;
        }
    }

    fs::remove_file(&zip_path).ok();
    Ok(())
}

// ── Bus loading ───────────────────────────────────────────────────────────────

fn load_into_bus(
    bus:           &mut SystemBus,
    roms_base:     &Path,
    load_order:    &[String],
    game_name:     &str,
) -> Result<(), String> {
    let game_dir  = roms_base.join(game_name);
    let _dep_dirs: Vec<PathBuf> = load_order.iter()
        .filter(|n| n.as_str() != game_name)
        .map(|n| roms_base.join(n))
        .collect();

    // System ROMs (BIOS + sfix + sm1 + lo) from host disk only.
    bus.load_host_system_roms();

    // M1 / Z80 program — game file preferred, fall back to host-loaded sm1.
    let m1_files = files_with_ext(&game_dir, "m1");
    if let Some(p) = m1_files.first() {
        bus.load_m1(p.to_str().unwrap_or_default()).ok();
    } else {
        load_m1_fallback(bus);
    }

    // P-ROM (68k program) — concatenate .p1, .p2, … in order.
    let p_data = collect_p_rom(&game_dir);
    if !p_data.is_empty() {
        // Install the cartridge handler before loading so process_p_rom() runs
        // with the correct implementation (e.g. Kof98Cart for kof98).
        bus.cart = cart::cart_for(game_name);
        bus.load_p_rom_bytes(p_data);
    }

    // ── Game fix-layer S-ROM (.s1) — cartridge tiles, separate from sfix ────
    if let Some(p) = files_with_ext(&game_dir, "s1").into_iter().next() {
        bus.load_s_rom(p.to_str().unwrap_or_default()).ok();
    }

    // ── C-ROMs (sprite tiles) — interleaved .c1+.c2, .c3+.c4, … ─────────
    let c_data = interleave_c_roms(&game_dir);
    if !c_data.is_empty() {
        bus.load_c_rom_bytes(c_data);
    }

    Ok(())
}

/// Return (ADPCM-A, ADPCM-B) sample data for a game.
///
/// MAME / FBNeo convention for cartridge V-ROMs:
///   * Files named `*.v1`, `*.v2`, `*.v3`, `*.v4` (single digit suffix) are
///     all concatenated into the ADPCM-A region in order.  For most NeoGeo
///     games (incl. KOF series, Metal Slug, etc.) this is the **only**
///     PCM region used — there is no separate ADPCM-B.  This is where
///     drums, voices, narration ("Ready!", "Go!"), and KOF-style
///     pre-streamed "music" samples live.
///   * Files named `*.v11`, `*.v12`, ... are ADPCM-A; `*.v21`, `*.v22` are
///     ADPCM-B (delta-T).  Only a handful of early games use this layout
///     (Magician Lord, NAM-1975, etc.).
pub fn collect_adpcm_roms_for(game_name: &str) -> (Vec<u8>, Vec<u8>) {
    let game_dir = PathBuf::from("roms").join(game_name);
    let mut a = Vec::new();
    let mut b = Vec::new();

    // Two-digit suffix layout (early games): v1x → ADPCM-A, v2x → ADPCM-B.
    let mut has_two_digit = false;
    for major in 1u8..=2 {
        for minor in 1u8..=9 {
            let ext = format!("v{}{}", major, minor);
            for path in files_with_ext(&game_dir, &ext) {
                has_two_digit = true;
                if let Ok(data) = fs::read(&path) {
                    let dst = if major == 1 { &mut a } else { &mut b };
                    let tag = if major == 1 { "ADPCM-A" } else { "ADPCM-B" };
                    log::info!("{}: loaded {} ({} bytes)", tag, path.display(), data.len());
                    dst.extend_from_slice(&data);
                }
            }
        }
    }
    if has_two_digit {
        return (a, b);
    }

    // Single-digit suffix layout (most cartridges): all v1..v4 → ADPCM-A.
    for n in 1u8..=4 {
        for path in files_with_ext(&game_dir, &format!("v{}", n)) {
            if let Ok(data) = fs::read(&path) {
                log::info!("ADPCM-A: loaded {} ({} bytes)", path.display(), data.len());
                a.extend_from_slice(&data);
            }
        }
    }
    (a, b)
}

fn load_m1_fallback(bus: &mut SystemBus) {
    // Fall back to system SM1 already loaded onto the bus (from disk), if any.
    if !bus.roms.sm1_rom.is_empty() {
        bus.load_m1_bytes(bus.roms.sm1_rom.clone());
    }
}

// ── ROM assembly helpers ──────────────────────────────────────────────────────

/// Concatenate P-ROM parts (.p1, .p2, … .p8) and the alternative .sp2 bank.
fn collect_p_rom(dir: &Path) -> Vec<u8> {
    let mut result = Vec::new();
    for n in 1u8..=8 {
        // Primary naming: .p1, .p2, …
        for path in files_with_ext(dir, &format!("p{}", n)) {
            log::debug!("collect_p_rom: found {:?}", path);
            if let Ok(data) = fs::read(&path) {
                result.extend_from_slice(&data);
            }
        }
        // Alternative: .sp2 is a common alias for the high-bank p2 file
        if n == 2 {
            for path in files_with_ext(dir, "sp2") {
                log::debug!("collect_p_rom: found {:?}", path);
                if let Ok(data) = fs::read(&path) {
                    result.extend_from_slice(&data);
                }
            }
        }
    }
    log::info!("collect_p_rom: total bytes loaded = {}", result.len());
    result
}

/// Interleave C-ROM pairs (.c1 + .c2 → bytes: c1[0], c2[0], c1[1], c2[1], …)
/// then concatenate the resulting interleaved blocks for all pairs.
///
/// The NeoGeo sprite tile format stores plane-0/1 bits in the C1 byte and
/// plane-2/3 bits in the C2 byte; byte-interleaving reconstructs the 128-byte
/// tile layout that the rest of the renderer expects.
fn interleave_c_roms(dir: &Path) -> Vec<u8> {
    let mut result = Vec::new();
    for pair in 1u8..=8 {
        let c_odd  = files_with_ext(dir, &format!("c{}", pair * 2 - 1));
        let c_even = files_with_ext(dir, &format!("c{}", pair * 2));
        match (c_odd.first(), c_even.first()) {
            (Some(p1), Some(p2)) => {
                log::debug!("interleave_c_roms: interleaving {:?} and {:?}", p1, p2);
                match (fs::read(p1), fs::read(p2)) {
                    (Ok(d1), Ok(d2)) => {
                        let len = d1.len().min(d2.len());
                        result.reserve(len * 2);
                        for i in 0..len {
                            result.push(d1[i]);
                            result.push(d2[i]);
                        }
                    }
                    _ => break,
                }
            }
            _ => break,
        }
    }
    result
}

// ── Filesystem utility ────────────────────────────────────────────────────────

/// Return all regular files in `dir` whose extension (case-insensitive) matches
/// `ext`, sorted alphabetically.
fn files_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(e) = path.extension().and_then(|e| e.to_str()) {
                    if e.eq_ignore_ascii_case(ext) {
                        files.push(path);
                    }
                }
            }
        }
    }
    files.sort();
    files
}
