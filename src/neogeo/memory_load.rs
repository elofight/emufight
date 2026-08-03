use super::Emulator;
use std::collections::HashMap;

/// Loads ROM bytes from an in-memory dictionary of files (filename -> bytes) into the Emulator.
pub fn load_roms_from_memory(
    emu: &mut Emulator,
    game_id: &str,
    files: &HashMap<String, Vec<u8>>,
) -> Result<(), String> {
    if files.is_empty() {
        return Err(format!(
            "NeoGeo: empty ROM file map for '{game_id}'"
        ));
    }
    // 0. Install the cartridge handler before loading
    emu.bus_mut().cart = super::cart::cart_for(game_id);

    // 1. BIOS, SFIX, SM1, LO
    // In neogeo.zip or similar.
    //
    // Multiple region BIOSes (sp-s2, sp-e, sp-j2, sp-u2, neopen, ...) are often
    // present in the file map simultaneously. Selecting one by HashMap
    // iteration order loads a different region every run — non-deterministic
    // boot and, worse, a guaranteed netplay desync between peers whose
    // iteration orders differ (the BIOS ROM is not part of the shared boot
    // savestate). Pick a single canonical BIOS deterministically: prefer the
    // standard MVS `sp-s2.sp1`, else the lexicographically-first `.sp1`, so
    // every run and every peer loads the identical BIOS.
    let mut loaded_bios = false;
    {
        let mut sp1: Vec<(&String, &Vec<u8>)> = files
            .iter()
            .filter(|(n, _)| n.to_lowercase().ends_with(".sp1"))
            .collect();
        sp1.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        let pick = sp1
            .iter()
            .find(|(n, _)| n.to_lowercase().ends_with("sp-s2.sp1"))
            .or_else(|| sp1.first());
        if let Some((name, data)) = pick {
            emu.bus_mut().load_bios_bytes(data).ok();
            loaded_bios = true;
            log::info!("neogeo: BIOS = {name} (deterministic canonical pick)");
        }
    }
    for (name, data) in files {
        let lower = name.to_lowercase();
        if lower.ends_with("sfix.sfix") {
            emu.bus_mut().load_sfix_bytes(data);
        } else if lower.ends_with("sm1.sm1") {
            emu.bus_mut().load_sm1_bytes(data);
        } else if lower.ends_with("000-lo.lo") {
            emu.bus_mut().load_lo_rom_bytes(data);
        }
    }

    // Fallback BIOS if none found but we have neogeo.zip extracted. Sort the
    // candidates so the pick stays deterministic (same reasoning as above).
    if !loaded_bios {
        let mut cand: Vec<(&String, &Vec<u8>)> = files
            .iter()
            .filter(|(n, _)| {
                let l = n.to_lowercase();
                l.contains("bios") || l.contains("sp-")
            })
            .collect();
        cand.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        if let Some((name, data)) = cand.first() {
            emu.bus_mut().load_bios_bytes(data).ok();
            log::info!("neogeo: BIOS fallback = {name} (deterministic)");
        }
    }

    // 2. M1 (Z80 program) and S1 (cart fix layer).
    //
    // A file map can contain stray/undersized entries — e.g. a 163-byte junk
    // file was observed masquerading as `.m1`/`.s1` and clobbering the real
    // 128 KB fix ROM, which blanks the fix layer (missing HUD / life-bar text).
    // Loading every match with last-writer-wins in HashMap order is both
    // non-deterministic and junk-susceptible. Pick the single LARGEST candidate
    // deterministically (tie-break by name) so the real ROM always wins on every
    // platform and every run.
    if let Some((name, data)) = files
        .iter()
        .filter(|(n, _)| {
            let l = n.to_lowercase();
            l.ends_with(".m1") && !l.ends_with("sm1.sm1")
        })
        .max_by(|a, b| a.1.len().cmp(&b.1.len()).then_with(|| b.0.cmp(a.0)))
    {
        log::info!("neogeo: M1 = {name} ({} bytes, deterministic largest pick)", data.len());
        emu.bus_mut().load_m1_bytes(data.clone());
    }
    if let Some((name, data)) = files
        .iter()
        .filter(|(n, _)| has_ext(n, "s1"))
        .max_by(|a, b| a.1.len().cmp(&b.1.len()).then_with(|| b.0.cmp(a.0)))
    {
        log::info!("neogeo: S1 = {name} ({} bytes, deterministic largest pick)", data.len());
        emu.bus_mut().load_s_rom_bytes(data.clone());
    }

    // 3. P ROM (required)
    let p_rom = collect_p_rom(files);
    if p_rom.is_empty() {
        return Err(format!(
            "NeoGeo: no program ROM (*.p1/…) found in file map for '{game_id}'"
        ));
    }
    let p_len = p_rom.len();
    emu.bus_mut().load_p_rom_bytes(p_rom);

    // 4. C ROM
    let c_rom = interleave_c_roms(files);
    let c_len = c_rom.len();
    if !c_rom.is_empty() {
        emu.bus_mut().load_c_rom_bytes(c_rom);
    }

    // 5. V ROM (ADPCM)
    let mut adpcm_a = Vec::new();
    let mut adpcm_b = Vec::new();

    let mut has_two_digit = false;
    for major in 1u8..=2 {
        for minor in 1u8..=9 {
            let ext = format!("v{}{}", major, minor);
            if let Some(data) = find_by_ext(files, &ext) {
                has_two_digit = true;
                let dst = if major == 1 { &mut adpcm_a } else { &mut adpcm_b };
                dst.extend_from_slice(data);
            }
        }
    }

    if !has_two_digit {
        for n in 1u8..=4 {
            if let Some(data) = find_by_ext(files, &format!("v{}", n)) {
                adpcm_a.extend_from_slice(data);
            }
        }
        // NeoGeo wires the V ROMs to both the ADPCM-A and ADPCM-B (delta-t)
        // buses; mirror the data so delta-t samples (e.g. kof98 voices) play.
        adpcm_b = adpcm_a.clone();
    }

    emu.load_adpcm_bytes(&adpcm_a, &adpcm_b);

    // ROM inventory summary: identical loader runs on native (disk-scanned map)
    // and wasm (uploaded map). Compare the two consoles to spot a partial/wrong
    // upload (e.g. a missing C-ROM pair = missing sprites like the life bar).
    let c_parts = (1u8..=8)
        .filter(|n| find_by_ext(files, &format!("c{n}")).is_some())
        .count();
    let s1_present = files.keys().any(|n| has_ext(n, "s1"));
    log::info!(
        "neogeo: ROM inventory {game_id}: files={} P={}B C={}B (c_parts={}/8) S1={} adpcm_a={}B adpcm_b={}B",
        files.len(),
        p_len,
        c_len,
        c_parts,
        s1_present,
        adpcm_a.len(),
        adpcm_b.len(),
    );
    Ok(())
}

fn has_ext(filename: &str, ext: &str) -> bool {
    let lower = filename.to_lowercase();
    let parts: Vec<&str> = lower.split('.').collect();
    parts.last().map(|&e| e == ext).unwrap_or(false)
}

fn find_by_ext<'a>(files: &'a HashMap<String, Vec<u8>>, ext: &str) -> Option<&'a Vec<u8>> {
    let mut matches: Vec<_> = files.iter().filter(|(k, _)| has_ext(k, ext)).collect();
    matches.sort_by(|a, b| a.0.cmp(b.0));
    matches.into_iter().next().map(|(_, data)| data)
}

fn collect_p_rom(files: &HashMap<String, Vec<u8>>) -> Vec<u8> {
    let mut result = Vec::new();
    for n in 1u8..=8 {
        let ext = format!("p{}", n);
        if let Some(data) = find_by_ext(files, &ext) {
            result.extend_from_slice(data);
        }
        if n == 2 {
            if let Some(data) = find_by_ext(files, "sp2") {
                result.extend_from_slice(data);
            }
        }
    }
    result
}

pub fn interleave_c_roms(files: &HashMap<String, Vec<u8>>) -> Vec<u8> {
    let mut result = Vec::new();
    for pair in 1u8..=8 {
        let odd_ext = format!("c{}", pair * 2 - 1);
        let even_ext = format!("c{}", pair * 2);

        let odd = find_by_ext(files, &odd_ext);
        let even = find_by_ext(files, &even_ext);

        match (odd, even) {
            (Some(d1), Some(d2)) => {
                let len = d1.len().min(d2.len());
                let start = result.len();
                result.resize(start + len * 2, 0);
                
                let dst = &mut result[start..start + len * 2];
                let src1 = &d1[..len];
                let src2 = &d2[..len];
                
                for i in 0..len {
                    dst[i * 2] = src1[i];
                    dst[i * 2 + 1] = src2[i];
                }
            }
            _ => break,
        }
    }
    result
}
