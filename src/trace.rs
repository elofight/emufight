//! Generic watchpoint helper driven by the `NEO_WATCH` env var.
//!
//! Format: `NEO_WATCH=R:D00100,W:10FD82,PC:9F30-9F50` (comma-separated).
//!   - `R:HHHHHH`   — log every read whose address matches
//!   - `W:HHHHHH`   — log every write whose address matches
//!   - `PC:HHHHHH`  — log when the 68K PC equals this address
//!   - `PC:LO-HI`   — log when PC ∈ [LO, HI] inclusive
//!
//! Hex values may optionally be prefixed with `$`. Calls are zero-cost when
//! the env var is unset or empty (early-return on `!active`).

use std::sync::OnceLock;

#[derive(Default)]
pub struct Watch {
    reads:  Vec<u32>,
    writes: Vec<u32>,
    pc:     Vec<(u32, u32)>,
    active: bool,
}

static WATCH: OnceLock<Watch> = OnceLock::new();

fn parse_hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim().trim_start_matches('$'), 16).ok()
}

fn build() -> Watch {
    let Ok(spec) = std::env::var("NEO_WATCH") else { return Watch::default(); };
    let mut w = Watch::default();
    for tok in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Some((kind, val)) = tok.split_once(':') else { continue; };
        match kind.trim() {
            "R" | "r" => if let Some(a) = parse_hex(val) { w.reads.push(a); },
            "W" | "w" => if let Some(a) = parse_hex(val) { w.writes.push(a); },
            "PC" | "pc" => {
                if let Some((lo, hi)) = val.split_once('-') {
                    if let (Some(l), Some(h)) = (parse_hex(lo), parse_hex(hi)) {
                        w.pc.push((l, h));
                    }
                } else if let Some(a) = parse_hex(val) {
                    w.pc.push((a, a));
                }
            }
            _ => {}
        }
    }
    w.active = !(w.reads.is_empty() && w.writes.is_empty() && w.pc.is_empty());
    if w.active {
        log::info!(
            "[watch] active — reads={:X?} writes={:X?} pc={:X?}",
            w.reads, w.writes, w.pc
        );
    }
    w
}

#[inline]
fn watch() -> &'static Watch {
    WATCH.get_or_init(build)
}

#[inline(always)]
pub fn check_read(addr: u32, val: u32, width: u8) {
    let w = watch();
    if !w.active { return; }
    if w.reads.iter().any(|&a| a == addr) {
        log::warn!("[watch] R{:<2} ${:06X} = ${:0w$X}", width, addr, val, w = (width as usize) / 4);
    }
}

#[inline(always)]
pub fn check_write(addr: u32, val: u32, width: u8) {
    let w = watch();
    if !w.active { return; }
    if w.writes.iter().any(|&a| a == addr) {
        log::warn!("[watch] W{:<2} ${:06X} = ${:0w$X}", width, addr, val, w = (width as usize) / 4);
    }
}

#[inline(always)]
pub fn check_pc(pc: u32) {
    let w = watch();
    if !w.active { return; }
    if w.pc.iter().any(|&(lo, hi)| pc >= lo && pc <= hi) {
        log::warn!("[watch] PC=${:06X}", pc);
    }
}
