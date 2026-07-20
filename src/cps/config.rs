//! CPS1 per-game configuration tables.
//!
//! Ported directly from MAME's `capcom/cps1_v.cpp` `CPS1config` table.
//! Only the two supported games (Street Fighter II World Warrior and
//! Champion Edition) are described here.
//!
//! The values are the raw hardware register offsets/behaviours needed to
//! drive the CPS-A (video) and CPS-B (priority/protection) custom chips.

/// GFX type bit flags (match MAME `GFXTYPE_*`).
pub const GFXTYPE_SPRITES: i32 = 0x01;
pub const GFXTYPE_SCROLL1: i32 = 0x02;
pub const GFXTYPE_SCROLL2: i32 = 0x04;
pub const GFXTYPE_SCROLL3: i32 = 0x08;

/// A single entry in a game's GFX bank mapping table.
///
/// `start`/`end` are expressed on the common 8x8-tile scale (as in MAME).
#[derive(Clone, Copy)]
pub struct GfxRange {
    pub gfxtype: i32,
    pub start: i32,
    pub end: i32,
    pub bank: usize,
}

/// CPS-B chip configuration (priority masks + protection registers).
#[derive(Clone, Copy)]
pub struct CpsBConfig {
    /// Self-test register (word offset into the CPS-B space) and expected value.
    pub cpsb_addr: i32,
    pub cpsb_value: i32,

    /// 16x16 multiply protection ports (byte offsets, -1 = unused).
    pub mult_factor1: i32,
    pub mult_factor2: i32,
    pub mult_result_lo: i32,
    pub mult_result_hi: i32,

    /// Byte offset of the layer-control register.
    pub layer_control: i32,
    /// Byte offsets of the four tile-group priority mask registers.
    pub priority: [i32; 4],
    /// Byte offset of the palette-control register.
    ///
    /// Retained for hardware accuracy: MAME uses this to gate which palette
    /// pages are DMA'd.  Our video core rebuilds every page each frame, so the
    /// value is not consulted yet.
    pub(crate) palette_control: i32,
    /// Layer-enable bit masks (index 0..4).
    pub layer_enable_mask: [i32; 5],
}

/// Full per-game description.
#[derive(Clone, Copy)]
pub struct CpsGame {
    pub name: &'static str,
    pub cpsb: CpsBConfig,
    /// GFX bank sizes (index 0..3) on the common tile scale.
    pub bank_sizes: [i32; 4],
    /// GFX bank mapping ranges (terminated implicitly by the slice length).
    pub bank_mapper: &'static [GfxRange],
    /// Byte offset (in CPS-B space) of the extra-input register (0 = none).
    pub in2_addr: i32,
    pub in3_addr: i32,
    /// Main-CPU clock in Hz (informational — used for cycle budgeting).
    pub cpu_clock: u32,
}

// STF29 / S9263B share an identical bank layout for these two games.
static MAPPER_SF2: [GfxRange; 6] = [
    GfxRange { gfxtype: GFXTYPE_SPRITES, start: 0x00000, end: 0x07fff, bank: 0 },
    GfxRange { gfxtype: GFXTYPE_SPRITES, start: 0x08000, end: 0x0ffff, bank: 1 },
    GfxRange { gfxtype: GFXTYPE_SPRITES, start: 0x10000, end: 0x11fff, bank: 2 },
    GfxRange { gfxtype: GFXTYPE_SCROLL3, start: 0x02000, end: 0x03fff, bank: 2 },
    GfxRange { gfxtype: GFXTYPE_SCROLL1, start: 0x04000, end: 0x04fff, bank: 2 },
    GfxRange { gfxtype: GFXTYPE_SCROLL2, start: 0x05000, end: 0x07fff, bank: 2 },
];

/// CPS_B_11 — Street Fighter II World Warrior.
const CPS_B_11: CpsBConfig = CpsBConfig {
    cpsb_addr: 0x32,
    cpsb_value: 0x0401,
    mult_factor1: -1,
    mult_factor2: -1,
    mult_result_lo: -1,
    mult_result_hi: -1,
    layer_control: 0x26,
    priority: [0x28, 0x2a, 0x2c, 0x2e],
    palette_control: 0x30,
    layer_enable_mask: [0x08, 0x10, 0x20, 0x00, 0x00],
};

/// CPS_B_21_DEF — Street Fighter II Champion Edition (with multiply protection).
const CPS_B_21_DEF: CpsBConfig = CpsBConfig {
    cpsb_addr: 0x32,
    cpsb_value: -1,
    mult_factor1: 0x00,
    mult_factor2: 0x02,
    mult_result_lo: 0x04,
    mult_result_hi: 0x06,
    layer_control: 0x26,
    priority: [0x28, 0x2a, 0x2c, 0x2e],
    palette_control: 0x30,
    layer_enable_mask: [0x02, 0x04, 0x08, 0x30, 0x30],
};

static GAMES: [CpsGame; 2] = [
    CpsGame {
        name: "sf2",
        cpsb: CPS_B_11,
        bank_sizes: [0x8000, 0x8000, 0x8000, 0],
        bank_mapper: &MAPPER_SF2,
        in2_addr: 0x36,
        in3_addr: 0,
        cpu_clock: 10_000_000,
    },
    CpsGame {
        name: "sf2ce",
        cpsb: CPS_B_21_DEF,
        bank_sizes: [0x8000, 0x8000, 0x8000, 0],
        bank_mapper: &MAPPER_SF2,
        in2_addr: 0x36,
        in3_addr: 0,
        cpu_clock: 12_000_000,
    },
];

/// Look up a game configuration by ROM-set name.
pub fn find(name: &str) -> Option<&'static CpsGame> {
    GAMES.iter().find(|g| g.name == name)
}

impl CpsGame {
    /// Remap a logical tile `code` of the given `gfxtype` to a physical
    /// position in the decoded 8x8-tile GFX buffer.
    ///
    /// Faithful port of MAME `cps_state::gfxrom_bank_mapper`.
    pub fn gfxrom_bank_mapper(&self, gfxtype: i32, code: i32) -> i32 {
        let shift = match gfxtype {
            GFXTYPE_SPRITES => 1,
            GFXTYPE_SCROLL1 => 0,
            GFXTYPE_SCROLL2 => 1,
            GFXTYPE_SCROLL3 => 3,
            _ => 0,
        };
        let code = code << shift;
        for range in self.bank_mapper {
            if (gfxtype & range.gfxtype) != 0 && code >= range.start && code <= range.end {
                let mut base = 0i32;
                for i in 0..range.bank {
                    base += self.bank_sizes[i];
                }
                let size = self.bank_sizes[range.bank];
                return (base + (code & (size - 1))) >> shift;
            }
        }
        -1
    }
}
