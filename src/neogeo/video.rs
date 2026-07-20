//! NeoGeo video pipeline — sprite + fix rendering into a 304×224 RGB24 framebuffer.
//!
//! `VideoController::render` operates on a zero-copy [`VideoSnapshot`] borrowed
//! from `SystemBus` — rendering happens **after** the frame's CPU cycles complete,
//! matching real hardware where the LSPC holds a double-buffered raster.
//!
//! # Layers (back to front)
//!
//! 1. **Background** — backdrop colour from palette 255, colour 15
//!    (`$401FFE` in the active bank)
//! 2. **Sprite layer** — up to 381 16×16 sprites from C-ROM, with zoom
//!    (shrink coefficients) and sprite chaining (sticky bit)
//! 3. **Fix layer** — 40×28 visible 8×8 tiles from VRAM `$7000–$74FF` + S-ROM
//!
//! The hitbox overlay is applied **after** `render` by the shell on a
//! presentation copy of the framebuffer, keeping the internal buffer clean
//! for save/rollback.

use crate::neogeo::bus::VideoSnapshot;

/// Active display width. The LSPC raster is 320px wide, but the outer 8px on
/// each side is CRT overscan hidden by the monitor bezel on real MVS hardware.
/// FBNeo declares all NeoGeo MVS games as 304×224 for the same reason.
pub const SCREEN_W: usize = 304;
pub const SCREEN_H: usize = 224;
/// Size of the RGB24 framebuffer in bytes.
pub const FRAMEBUFFER_BYTES: usize = SCREEN_W * SCREEN_H * 3;

// Fix layer geometry
const FIX_COLS: usize = 40;
/// Visible fix-layer rows (NTSC): VRAM rows 2–29 of the 32-row map.
/// Rows 0–1 are hidden overscan at the top; rows 30–31 at the bottom.
const FIX_ROWS: usize = 28;
const FIX_ROW_OFFSET: usize = 2;   // first visible VRAM row
/// First *visible* fix column (column 0 is left overscan, hidden on real HW).
const FIX_COL_FIRST: usize = 1;
/// VRAM word address of fix-map column 0, row 0.
const FIX_VRAM_BASE: usize = 0x7000;
/// Word stride between columns (32 rows/column).
const FIX_COL_STRIDE: usize = 0x20;

pub struct VideoController {
    pub frame_count: usize,
}

impl VideoController {
    pub fn new() -> Self {
        Self { frame_count: 0 }
    }

    /// Render one full frame into `framebuffer` using a zero-copy snapshot of bus state.
    ///
    /// Obtain `snap` via `SystemBus::video_snapshot()`.  The two borrows
    /// (`&mut self.video` and `&self.bus`) come from different fields so the
    /// borrow checker accepts them on the same line.
    ///
    /// Hitbox overlay (if enabled) is applied *after* step by the shell via
/// (`crate::hitbox::draw`) on a presentation copy.  This keeps the internal framebuffer
/// clean for save/rollback
    /// and ensures zero cost when the overlay is disabled.
    pub fn render(&mut self, framebuffer: &mut [u8], snap: &VideoSnapshot<'_>) {
        let VideoSnapshot { vram, pal_ram, sfix_rom, s_rom, c_rom, pal_bank, brd_fix, auto_anim_frame, lo_rom, shadow } = snap;
        let (pal_bank, brd_fix, auto_anim_frame, shadow) = (*pal_bank, *brd_fix, *auto_anim_frame, *shadow);
        debug_assert_eq!(framebuffer.len(), FRAMEBUFFER_BYTES);
        self.frame_count += 1;

        // Choose the active fix-tile source.
        // Prefer the cartridge S-ROM when CRTFIX is active and the game has
        // loaded one; fall back to the system SFIX so the BIOS always has tiles.
        let fix_rom: &[u8] = if brd_fix || s_rom.is_empty() {
            sfix_rom
        } else {
            s_rom
        };

        // ── 1. Background fill ────────────────────────────────────────────────
        let (br, bg, bb) = palette_color(pal_ram, pal_bank, 255, 15, shadow);
        for chunk in framebuffer.chunks_exact_mut(3) {
            chunk[0] = br;
            chunk[1] = bg;
            chunk[2] = bb;
        }

        // ── 2. Sprite layer ───────────────────────────────────────────────────
        // Sprites are rendered before the fix layer (fix is always on top).
        if !c_rom.is_empty() {
            draw_sprites(framebuffer, vram, pal_ram, pal_bank, c_rom, auto_anim_frame, lo_rom, shadow);
        }

        // ── 3. Fix layer ──────────────────────────────────────────────────────
        if fix_rom.is_empty() {
            return;
        }

        for col in FIX_COL_FIRST..FIX_COLS {
            for row in 0..FIX_ROWS {
                // Skip the 2 hidden overscan rows at the top of the VRAM map.
                // VRAM rows 0–1 are not displayed on NTSC; the visible rows are 2–29.
                let vram_row  = row + FIX_ROW_OFFSET;
                let word_addr = FIX_VRAM_BASE + col * FIX_COL_STRIDE + vram_row;
                let byte_off  = word_addr * 2;
                if byte_off + 1 >= vram.len() { continue; }

                let cell = ((vram[byte_off] as u16) << 8) | vram[byte_off + 1] as u16;
                let tile_num = (cell & 0x0FFF) as usize;
                let pal_idx  = ((cell >> 12) & 0xF) as usize;

                // Offset screen X by FIX_COL_FIRST to skip the left overscan column.
                draw_fix_tile(
                    framebuffer, fix_rom, pal_ram, pal_bank,
                    tile_num, pal_idx,
                    (col - FIX_COL_FIRST) * 8, row * 8,
                    shadow,
                );
            }
        }
    }

}

// ── Fix tile rasteriser ───────────────────────────────────────────────────────

/// Rasterise one 8×8 S-ROM fix tile at screen position (`sx`, `sy`).
///
/// S-ROM tile format (32 bytes / tile, 4 bytes / row):
///   The 32-bit row word is formed from 4 consecutive ROM bytes (big-endian).
///   Pixel columns within the word — GnGeo reference decoding (little-endian):
///     px0 = low nibble byte0  → bits 27..24 of BE word  (shift 24)
///     px1 = high nibble byte0 → bits 31..28              (shift 28)
///     px2 = low nibble byte1  → bits 19..16              (shift 16)
///     px3 = high nibble byte1 → bits 23..20              (shift 20)
///     px4 = low nibble byte2  → bits 11..8               (shift  8)
///     px5 = high nibble byte2 → bits 15..12              (shift 12)
///     px6 = low nibble byte3  → bits  3..0               (shift  0)
///     px7 = high nibble byte3 → bits  7..4               (shift  4)
///   Color index 0 is transparent (does not overwrite the framebuffer).
fn draw_fix_tile(
    framebuffer: &mut [u8],
    sfix_rom:    &[u8],
    pal_ram:     &[u8],
    pal_bank:    bool,
    tile_num:    usize,
    pal_idx:     usize,
    sx:          usize,
    sy:          usize,
    shadow:      bool,
) {
    // Raw S-ROM tile format (from FBNeo NeoTextDecodeTile):
    // 32 bytes/tile stored as 4 strips of 8 bytes each (NOT row-major):
    //   Bytes  0.. 7: strip0 — row y → byte[y]:    low nibble=px4, high nibble=px5
    //   Bytes  8..15: strip1 — row y → byte[8+y]:  low nibble=px6, high nibble=px7
    //   Bytes 16..23: strip2 — row y → byte[16+y]: low nibble=px0, high nibble=px1
    //   Bytes 24..31: strip3 — row y → byte[24+y]: low nibble=px2, high nibble=px3

    let tile_base = tile_num * 32;
    if tile_base + 31 >= sfix_rom.len() { return; }

    for ty in 0..8_usize {
        let py = sy + ty;
        if py >= SCREEN_H { continue; }

        for tx in 0..8_usize {
            let strip_off = match tx {
                0 | 1 => 16,
                2 | 3 => 24,
                4 | 5 =>  0,
                _     =>  8,
            };
            let byte = sfix_rom[tile_base + strip_off + ty];
            let color_idx = if tx % 2 == 0 {
                (byte & 0xF) as usize       // even pixel = low nibble
            } else {
                ((byte >> 4) & 0xF) as usize // odd pixel = high nibble
            };
            if color_idx == 0 { continue; } // transparent

            let px = sx + tx;
            if px >= SCREEN_W { continue; }

            let (r, g, b) = palette_color(pal_ram, pal_bank, pal_idx, color_idx, shadow);
            let i = (py * SCREEN_W + px) * 3;
            framebuffer[i]     = r;
            framebuffer[i + 1] = g;
            framebuffer[i + 2] = b;
        }
    }
}

// ── Palette decoder ───────────────────────────────────────────────────────────

/// Decode one NeoGeo palette entry to RGB24.
///
/// 16-bit entry layout (per NeoGeo hardware spec):
///   bit 15:     dark bit — activates an 8200Ω pulldown, reducing each
///               channel by ~1.5% (MAME neogeo_v.cpp `weights_dark`).
///               This is NOT a halving; it is the shared LSB for fine tuning.
///   bit 14:     R[0]  (red LSB)
///   bit 13:     G[0]  (green LSB)
///   bit 12:     B[0]  (blue LSB)
///   bits 11:8:  R\[4:1\] (red MSBs)
///   bits 7:4:   G\[4:1\] (green MSBs)
///   bits 3:0:   B\[4:1\] (blue MSBs)
///
/// Each channel is 5 bits: R5 = {R\[4:1\], R[0]}.
/// Expanded to 8 bits: R8 = (R5 << 3) | (R5 >> 2).
///
/// `shadow` — global dim mode (REG_SHADOW / 0x3A0011).  Activates a 150Ω
/// pulldown across the entire DAC, reducing all channels to ~55.7% brightness
/// (MAME `weights_shadow`; FBNeo: "nearly halfen in brightness").
fn palette_color(
    pal_ram:   &[u8],
    pal_bank:  bool,
    pal_idx:   usize,
    color_idx: usize,
    shadow:    bool,
) -> (u8, u8, u8) {
    let bank_base = if pal_bank { 0x2000 } else { 0 };
    let offset    = bank_base + (pal_idx * 16 + color_idx) * 2;
    if offset + 1 >= 0x4000 { return (0, 0, 0); }

    let hi = pal_ram[offset]     as u16;
    let lo = pal_ram[offset + 1] as u16;
    let e  = (hi << 8) | lo;

    let dark = (e & 0x8000) != 0;
    // 5-bit channels: MSBs at bits\[11:8\]/\[7:4\]/\[3:0\], LSBs at bits[14]/[13]/[12]
    let r5 = (((e >>  8) & 0xF) << 1) | ((e >> 14) & 1);
    let g5 = (((e >>  4) & 0xF) << 1) | ((e >> 13) & 1);
    let b5 = (((e >>  0) & 0xF) << 1) | ((e >> 12) & 1);
    // Expand 5-bit to 8-bit: x8 = (x5 << 3) | (x5 >> 2)
    let r = ((r5 << 3) | (r5 >> 2)) as u8;
    let g = ((g5 << 3) | (g5 >> 2)) as u8;
    let b = ((b5 << 3) | (b5 >> 2)) as u8;

    // Per-palette dark bit: 8200Ω pulldown ≈ 1.5% reduction (255→251 at max).
    let mut r = if dark { (r as u16 * 251 / 255) as u8 } else { r };
    let mut g = if dark { (g as u16 * 251 / 255) as u8 } else { g };
    let mut b = if dark { (b as u16 * 251 / 255) as u8 } else { b };
    // Global shadow mode: 150Ω pulldown ≈ 55.7% of normal brightness.
    if shadow {
        r = (r as u16 * 142 / 255) as u8;
        g = (g as u16 * 142 / 255) as u8;
        b = (b as u16 * 142 / 255) as u8;
    }
    (r, g, b)
}

// ── Sprite layer ──────────────────────────────────────────────────────────────

/// NeoGeo LSPC-2 sprite system.
///
/// VRAM layout (word addresses):
///   Slow VRAM 0x0000–0x5F7F  SCB1: 381 sprites × 32 tile-rows × 2 words
///   Slow VRAM 0x7000–0x74FF  Fix-layer tile map  (handled elsewhere)
///   Fast VRAM 0x8000–0x81FD  SCB2: shrink coefficients (381 entries × 1 word)
///   Fast VRAM 0x8200–0x83FD  SCB3: Y position + sticky + height
///   Fast VRAM 0x8400–0x85FD  SCB4: X position
///
/// SCB1 entry for sprite S, tile-row T  (2 words starting at word addr S×64+T×2):
///   Even word (word 0): bits\[15:0\] = tile_code\[15:0\]
///   Odd  word (word 1): bits\[15:12\]=palette, bits\[11:8\]=tile_code\[19:16\],
///                       bit[7]=8-frame auto-anim, bit[6]=4-frame auto-anim,
///                       bit[3]=V-flip, bit[2]=H-flip
///   20-bit tile code = {odd\[11:8\], even\[15:0\]}
///
/// SCB2 (fast VRAM word 0x8000+S):  bits\[11:8\]=H-shrink, bits\[7:0\]=V-shrink
/// SCB3 (fast VRAM word 0x8200+S):  bits\[15:7\]=Y-pos, bit[6]=sticky, bits\[5:0\]=height
/// SCB4 (fast VRAM word 0x8400+S):  bits\[15:7\]=X-pos (screen_x = x_pos − 0x160)
///
/// C-ROM tile format (128 bytes per 16×16 tile, C1+C2 interleaved per byte):
///   Raw layout (from GnGeo convert_roms_tile):
///     Bytes   0..63  = rows 0-15, pixels  8-15 (right half), 4 bytes/row
///     Bytes  64..127 = rows 0-15, pixels  0-7  (left  half), 4 bytes/row
///   Within each 4-byte group at base = row*4 (or 64+row*4):
///     byte[base+0] (even=C1): bit k → plane0 of pixel (half_start + k)
///     byte[base+1] (odd =C2): bit k → plane2 of pixel
///     byte[base+2] (even=C1): bit k → plane1 of pixel
///     byte[base+3] (odd =C2): bit k → plane3 of pixel
///   bit 0 (LSB) = leftmost pixel of the group.  color==0 is transparent.
fn draw_sprites(
    framebuffer:     &mut [u8],
    vram:            &[u8],
    pal_ram:         &[u8],
    pal_bank:        bool,
    c_rom:           &[u8],
    auto_anim_frame: u8,
    lo_rom:          &[u8],
    shadow:          bool,
) {
    // Fast VRAM starts at byte offset 0x10000 in the vram Vec.
    const FAST_BASE: usize = 0x10000;

    // Sprite-chain state.  Per neogeodev wiki (sticky bit / Sprites):
    //   Setting SCB3 bit 6 makes the sprite "stick to the right of the
    //   previous one".  Chained sprites INHERIT the chain head's Y position,
    //   height, and *vertical* shrink (V-shrink); each successive sticky
    //   sprite is placed at  prev_x + prev_drawn_width  (NOT a flat +16:
    //   if the previous sprite is H-shrunk, the next sits next to it).
    //   Only SCB1 (tile data) and the H-shrink coefficient of SCB2 stay
    //   per-sprite.
    let mut prev_x_raw:      i32   = 0;   // raw X (0..511) of previous sprite
    let mut prev_drawn_w:    i32   = 16;  // H-shrunk width of previous sprite
    let mut head_y_top:     i32   = 0;   // chain-head screen-space top Y
    let mut head_rows:      usize = 0;   // chain-head height (source tile rows)
    let mut head_dest_h:    i32   = 0;   // chain-head destination pixel height
    let mut head_vshrink:   u32   = 255; // chain-head V-shrink (sticky sprites inherit)

    for s in 0..381_usize {
        // ── Read SCB3 (Y position, sticky, height) ──
        let scb3_off = FAST_BASE + 0x400 + s * 2;
        if scb3_off + 1 >= vram.len() { continue; }
        let scb3 = ((vram[scb3_off] as u16) << 8) | vram[scb3_off + 1] as u16;
        let sticky = (scb3 & 0x40) != 0;

        // ── Read SCB4 (X position) ──
        let scb4_off = FAST_BASE + 0x800 + s * 2;
        if scb4_off + 1 >= vram.len() { continue; }
        let scb4 = ((vram[scb4_off] as u16) << 8) | vram[scb4_off + 1] as u16;

        // ── Read SCB2 (zoom coefficients) ──
        // Word $8000+s = byte offset FAST_BASE + s*2.
        let scb2_off = FAST_BASE + s * 2;
        if scb2_off + 1 >= vram.len() { continue; }
        let scb2 = ((vram[scb2_off] as u16) << 8) | vram[scb2_off + 1] as u16;
        let hshrink     = ((scb2 >> 8) & 0x0F) as u32;  // 0..15 (15 = full)
        let vshrink_own = (scb2 & 0xFF) as u32;          // 0..255 (255 = full)
        let dest_w: i32 = (hshrink + 1) as i32;

        // Resolve geometry: chain head reads its own SCB3/SCB4; sticky
        // sprites inherit Y, height, and V-shrink from the head.
        // FBNeo: nBankYZoom only updated in the non-sticky branch, so sticky
        // sprites use the chain-head's V-shrink for the LO ROM lookup.
        let (sx_raw, sy_top, rows, dest_h, vshrink) = if sticky {
            let nx = (prev_x_raw + prev_drawn_w) & 0x1FF;
            (nx, head_y_top, head_rows, head_dest_h, head_vshrink)
        } else {
            let height_raw = (scb3 & 0x3F) as usize;
            let y_raw = ((scb3 >> 7) as i32) & 0x1FF;
            let x_raw = ((scb4 >> 7) as i32) & 0x1FF;
            let r = if height_raw == 0 { 0 } else { height_raw.min(32) };
            // Destination pixel height: full = rows * 16; shrink coeff
            // (vshrink+1)/256 scales it.
            let dh = (((vshrink_own + 1) * (r as u32) * 16) >> 8) as i32;
            // Y position: the LSPC field is the screen-line of the top
            // of the sprite, expressed in the 9-bit virtual frame.
            //   y_top = 0x1F0 - y_raw
            // Treating it as the bottom of the sprite (per the
            // neogeodev wiki text) caused every chain head to drift up
            // by its own height — tall BG strips disappeared off the
            // top while short HUD strips looked OK.
            let y_top    = 0x1F0 - y_raw;
            head_y_top  = y_top;
            head_rows   = r;
            head_dest_h = dh;
            head_vshrink = vshrink_own;
            (x_raw, y_top, r, dh, vshrink_own)
        };
        // Always update prev_x / prev_drawn_w for the next chain step.
        prev_x_raw   = sx_raw;
        prev_drawn_w = dest_w;
        if rows == 0 || dest_h == 0 || dest_w == 0 { continue; }

        // Map raw 9-bit X to signed screen X, matching FBNeo's 304-wide MVS
        // mode: subtract 8 to align sprite coordinates with the 304px active
        // area (sprite X=8 → screen X=0), then wrap values >= 0x1E0 to the
        // negative left-overscan range (-32..-1), matching FBNeo neo_sprite.cpp.
        let sx_adj = (sx_raw - 8).rem_euclid(512);
        let sx: i32 = if sx_adj >= 0x1E0 { sx_adj - 0x200 } else { sx_adj };
        if sx >= SCREEN_W as i32 || sx + dest_w <= 0 { continue; }

        // Per-scanline rasteriser.
        //
        // If the 000-lo.lo hardware LUT is loaded, use it for bit-exact
        // V-shrink: `lo_rom[vshrink * 256 + dest_y]` encodes the source
        // tile and row as `(src_tile << 4) | src_row`; `0xFF` = skip line.
        // Fall back to linear interpolation when the LUT is absent.
        //
        // Hardware-accurate iteration count: the LSPC-2 always iterates
        // rows×16 output scanlines regardless of the V-shrink coefficient.
        // The LO ROM maps each output scanline to a (tile, row) pair —
        // every byte is two nibbles; there is NO skip/transparent marker.
        // 0xFF simply encodes tile=15, row=15 (the last line of the last
        // tile) and must be rendered like any other entry.
        // Using the pre-computed dest_h (< rows×16 for vshrink < 255) as the
        // loop bound would cut the sprite short by the difference, producing
        // a visible gap at the bottom of background strips.
        let src_h = (rows as i32) * 16;
        let hw_lines = if lo_rom.is_empty() { dest_h } else { src_h };
        for dy in 0..hw_lines {
            // Y wrap mod-512 in hardware.
            let sy_raw = sy_top + dy;
            let sy_mod = ((sy_raw % 512) + 512) % 512;
            let sy = if sy_mod >= 256 { sy_mod - 512 } else { sy_mod };
            if sy < 0 || sy >= SCREEN_H as i32 { continue; }

            let (t, y_in_tile) = if !lo_rom.is_empty() {
                let lut_idx = (vshrink as usize) * 256 + (dy as usize);
                let lut_val = lo_rom.get(lut_idx).copied().unwrap_or(0);
                ((lut_val >> 4) as usize, (lut_val & 0x0F) as usize)
            } else {
                let src_y_full = ((dy * src_h) / dest_h) as usize;
                (src_y_full / 16, src_y_full % 16)
            };
            if t >= rows { continue; }

            // ── Read SCB1 for tile-row t ──
            let word_addr_0 = s * 64 + t * 2;
            let b0 = word_addr_0 * 2;
            let b1 = b0 + 2;
            if b1 + 1 >= vram.len() { continue; }
            let w0 = ((vram[b0] as u16) << 8) | vram[b0 + 1] as u16;
            let w1 = ((vram[b1] as u16) << 8) | vram[b1 + 1] as u16;
            // SCB1 odd word layout:
            //   bits\[15:12\] = palette index (4 bits)
            //   bits\[11:8\]  = tile_code\[19:16\] (4 bits)
            //   bits\[15:8\]  = palette index (0-255, 8-bit)
            //   bits\[7:4\]   = tile code extension (tile_hi, bits 16-19)
            //   bit[3]      = 8-frame auto-animation enable
            //   bit[2]      = 4-frame auto-animation enable
            //   bit[1]      = vertical flip
            //   bit[0]      = horizontal flip
            let tile_lo   = w0 as usize;
            let palette   = ((w1 >>  8) & 0xFF) as usize;
            let tile_hi   = ((w1 >>  4) & 0x0F) as usize;
            let auto8     = (w1 & 0x0008) != 0;
            let auto4     = (w1 & 0x0004) != 0;
            let vflip     = (w1 & 0x0002) != 0;
            let hflip     = (w1 & 0x0001) != 0;
            // Apply auto-animation: substitute low tile-code bits with the
            // current animation frame counter, matching LSPC-2 hardware behaviour.
            let mut tile_code = (tile_hi << 16) | tile_lo;
            if auto8 {
                tile_code = (tile_code & !0x07) | (auto_anim_frame as usize & 0x07);
            } else if auto4 {
                tile_code = (tile_code & !0x03) | (auto_anim_frame as usize & 0x03);
            }

            draw_sprite_scanline(
                framebuffer, c_rom, pal_ram, pal_bank,
                tile_code, palette,
                sx, sy, y_in_tile, dest_w,
                hflip, vflip, shadow,
            );
        }
    }
}

/// Rasterise one scanline of a (possibly H-shrunk) sprite tile.
///
/// `y_in_tile` selects the source pixel row (0..15) within the 16×16 tile.
/// `dest_w` is the horizontal output width in pixels (1..16). Source columns
/// are picked by linear nearest-neighbour `src_x = dest_x * 16 / dest_w`.
fn draw_sprite_scanline(
    framebuffer: &mut [u8],
    c_rom:       &[u8],
    pal_ram:     &[u8],
    pal_bank:    bool,
    tile_code:   usize,
    pal_idx:     usize,
    sx:          i32,
    sy:          i32,
    y_in_tile:   usize,
    dest_w:      i32,
    hflip:       bool,
    vflip:       bool,
    shadow:      bool,
) {
    let tile_base = tile_code * 128;
    if tile_base + 127 >= c_rom.len() { return; }
    let ty = if vflip { 15 - y_in_tile } else { y_in_tile };

    for dx in 0..dest_w {
        let px = sx + dx;
        if px < 0 || px >= SCREEN_W as i32 { continue; }
        let src_x = ((dx * 16) / dest_w) as usize;
        let tx = if hflip { 15 - src_x } else { src_x };

        let bit  = tx % 8;
        let base = if tx < 8 { 64 + ty * 4 } else { ty * 4 };

        let p0 = (c_rom[tile_base + base    ] >> bit) & 1;
        let p2 = (c_rom[tile_base + base + 1] >> bit) & 1;
        let p1 = (c_rom[tile_base + base + 2] >> bit) & 1;
        let p3 = (c_rom[tile_base + base + 3] >> bit) & 1;

        let color_idx = ((p3 << 3) | (p2 << 2) | (p1 << 1) | p0) as usize;
        if color_idx == 0 { continue; }

        let (r, g, b) = palette_color(pal_ram, pal_bank, pal_idx, color_idx, shadow);
        let i = (sy as usize * SCREEN_W + px as usize) * 3;
        framebuffer[i]     = r;
        framebuffer[i + 1] = g;
        framebuffer[i + 2] = b;
    }
}

