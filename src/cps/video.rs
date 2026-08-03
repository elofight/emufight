//! CPS1 video renderer.
//!
//! Renders the 384×224 CPS1 display from graphics RAM, the CPS-A base/scroll
//! registers and the CPS-B priority/layer-control registers.  The pipeline
//! mirrors MAME `capcom/cps1_v.cpp`:
//!
//! * three scrolling tile layers (8×8 / 16×16 / 32×32),
//! * hardware sprites (16×16 cells, block-composed),
//! * a 6-page (0xC00-entry) colour palette with per-colour brightness,
//! * per-tile priority masks that let selected pens sit above the sprites.
//!
//! The GFX tile decode uses the CPS1 4-bpp planar layout (see `tile_pixel`):
//! each tile row stores four bit-planes (little-endian) with pixels MSB-first,
//! matching the ROM_LOAD64_WORD-assembled region produced by `romset`.

use super::bus::CpsBus;

pub const WIDTH: usize = 384;
pub const HEIGHT: usize = 224;
pub const FRAMEBUFFER_BYTES: usize = WIDTH * HEIGHT * 3;

// CPS-A register word indices.
const OBJ_BASE: usize = 0x00 / 2;
const SCROLL1_BASE: usize = 0x02 / 2;
const SCROLL2_BASE: usize = 0x04 / 2;
const SCROLL3_BASE: usize = 0x06 / 2;
const OTHER_BASE: usize = 0x08 / 2;
const PALETTE_BASE: usize = 0x0a / 2;
const SCROLL1_X: usize = 0x0c / 2;
const SCROLL1_Y: usize = 0x0e / 2;
const SCROLL2_X: usize = 0x10 / 2;
const SCROLL2_Y: usize = 0x12 / 2;
const SCROLL3_X: usize = 0x14 / 2;
const SCROLL3_Y: usize = 0x16 / 2;
const ROWSCROLL_OFFS: usize = 0x20 / 2;
const VIDEOCONTROL: usize = 0x22 / 2;

// Video-base boundaries (bytes).
const SCROLL_SIZE: u32 = 0x4000;
const OBJ_SIZE: u32 = 0x0800;
const OTHER_SIZE: u32 = 0x0800;
const PALETTE_ALIGN: u32 = 0x0400;

// GFX type flags (mirror config).
use super::config::{GFXTYPE_SCROLL1, GFXTYPE_SCROLL2, GFXTYPE_SCROLL3, GFXTYPE_SPRITES};

// Priority level applied to tile pens flagged "above sprites".
const HIGH_LEVEL: u8 = 200;

// CPS1 visible-area origin within the 512-wide render bitmap (MAME cps1.h:
// CPS_HBEND=64, CPS_VBEND=16). Screen pixel (sx,sy) == bitmap pixel
// (sx+64, sy+16); tilemaps sample world (scroll + this offset) and sprites
// are placed at their raw bitmap coordinate minus this offset.
const SCREEN_X_OFF: i32 = 64;
const SCREEN_Y_OFF: i32 = 16;

pub struct CpsVideo {
    pub framebuffer: Vec<u8>,
    /// CPS1 4-bpp planar tile graphics (ROM_LOAD64_WORD-assembled region).
    gfx: Vec<u8>,
    color: Vec<u16>,
    pri: Vec<u8>,
    palette: Vec<(u8, u8, u8)>,
    /// One-frame-delayed snapshot of the sprite object list.
    ///
    /// CPS1 hardware buffers the OBJ table: it is latched once per frame at the
    /// start of vblank and displayed one frame late while the CPU builds the
    /// next list into the live OBJ RAM (typically via a ping-pong of the OBJ
    /// base register).  Rendering directly from the live list instead catches
    /// half-built frames — producing flickering/duplicated sprites and stray
    /// garbage blocks.  We snapshot the OBJ region here (see
    /// `CpsVideo::buffer_sprites`) and draw sprites from it.
    buffered_obj: Vec<u16>,
}

impl CpsVideo {
    pub fn new() -> Self {
        CpsVideo {
            framebuffer: vec![0u8; FRAMEBUFFER_BYTES],
            gfx: Vec::new(),
            color: vec![0u16; WIDTH * HEIGHT],
            pri: vec![0u8; WIDTH * HEIGHT],
            palette: vec![(0, 0, 0); 0xc00],
            buffered_obj: vec![0u16; (OBJ_SIZE / 2) as usize],
        }
    }

    pub fn set_gfx(&mut self, gfx: Vec<u8>) {
        self.gfx = gfx;
    }

    /// Snapshot the latched sprite OBJ table for save states.  All other video
    /// buffers (palette, colour, priority, framebuffer) are derived each render.
    pub fn snapshot_obj(&self) -> Vec<u16> {
        self.buffered_obj.clone()
    }

    /// Restore the latched sprite OBJ table from a save state.
    pub fn restore_obj(&mut self, obj: &[u16]) {
        let n = obj.len().min(self.buffered_obj.len());
        self.buffered_obj[..n].copy_from_slice(&obj[..n]);
    }

    /// Latch the sprite object list for this frame (MAME `screen_vblank_cps1`:
    /// `memcpy(m_buffered_obj, m_obj, m_obj_size)`).  Call once per frame at the
    /// vblank boundary — before the CPU rebuilds the list — so the rendered
    /// sprites come from a stable, complete snapshot.
    pub fn buffer_sprites(&mut self, bus: &CpsBus) {
        let base = bus.video_base(OBJ_BASE, OBJ_SIZE);
        for (i, slot) in self.buffered_obj.iter_mut().enumerate() {
            *slot = bus.gfxram.get(base + i).copied().unwrap_or(0);
        }
    }

    // ── Palette ──────────────────────────────────────────────────────────────

    fn build_palette(&mut self, bus: &CpsBus) {
        let base = bus.video_base(PALETTE_BASE, PALETTE_ALIGN);
        // MAME cps1_build_palette: only the palette pages enabled in the CPS-B
        // palette_control register are copied from gfxram, and skipped *leading*
        // pages compact the following ones (the source pointer only advances
        // once at least one page has been copied). A flat 1:1 copy misaligns the
        // per-colour ramps and produces speckled text.
        let pc = bus.game().cpsb.palette_control;
        let ctrl = if pc < 0 {
            0x3f
        } else {
            bus.cps_b[((pc / 2) as usize) & 0x1f]
        };
        let mut src = base; // gfxram word offset
        let mut copied_any = false;
        for page in 0..6usize {
            if ctrl & (1 << page) != 0 {
                for offset in 0..0x200usize {
                    let p = bus.gfxram.get(src + offset).copied().unwrap_or(0) as u32;
                    let bright = 0x0f + ((p >> 12) << 1); // 0x0f..0x2d
                    let r = (((p >> 8) & 0xf) * 0x11 * bright / 0x2d).min(255) as u8;
                    let g = (((p >> 4) & 0xf) * 0x11 * bright / 0x2d).min(255) as u8;
                    let b = ((p & 0xf) * 0x11 * bright / 0x2d).min(255) as u8;
                    self.palette[page * 0x200 + offset] = (r, g, b);
                }
                src += 0x200;
                copied_any = true;
            } else if copied_any {
                src += 0x200;
            }
        }
    }

    // ── Tile pixel fetch ─────────────────────────────────────────────────────

    #[inline]
    fn tile_pixel(&self, size: usize, phys_code: i32, x: usize, y: usize, gfxset: usize) -> u8 {
        if phys_code < 0 {
            return 15;
        }
        // Exact port of MAME `capcom/cps1.cpp` gfx layouts (standard gfx_layout,
        // planeoffset {24,16,8,0}): pen bit `b` (b=0..3, LSB first) is stored in
        // byte `+b`, and pixel `x` uses bit `7-x` of that byte (MSB = leftmost).
        //   cps1_layout8x8   : 64-byte block; gfxset 0 = bytes 0-3, gfxset 1 = 4-7
        //   cps1_layout16x16 : 128-byte block; left 8px = +0..3, right 8px = +4..7
        //   cps1_layout32x32 : 512-byte block; each 8px column group = +q*4
        let code = phys_code as usize;
        let (base, xb) = match size {
            8 => (code * 64 + y * 8 + gfxset * 4, x),
            16 => (code * 128 + y * 8 + if x >= 8 { 4 } else { 0 }, x & 7),
            _ => (code * 512 + y * 16 + (x >> 3) * 4, x & 7),
        };
        let shift = 7 - xb;
        let mut pen = 0u8;
        for b in 0..4 {
            match self.gfx.get(base + b) {
                Some(&byte) => pen |= ((byte >> shift) & 1) << b,
                None => return 15,
            }
        }
        pen
    }

    #[inline]
    fn put(&mut self, x: usize, y: usize, palidx: u16, level: u8) {
        let i = y * WIDTH + x;
        if level >= self.pri[i] {
            self.pri[i] = level;
            self.color[i] = palidx;
        }
    }

    // ── Tile layers ──────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn draw_tilemap(
        &mut self,
        bus: &CpsBus,
        size: usize,
        base: usize,
        scrollx: i32,
        scrolly: i32,
        color_add: u16,
        gfxtype: i32,
        code_mask: u16,
        prio_reg: u16,
        slot_level: u8,
        rowscroll: Option<(usize, i32)>,
    ) {
        let mask = (size - 1) as i32;
        for sy in 0..HEIGHT {
            let wy = sy as i32 + SCREEN_Y_OFF + scrolly;
            let row = wy.div_euclid(size as i32);
            let ty = (wy & mask) as usize;
            // Per-row horizontal scroll for scroll2 line-scroll (MAME:
            // screen line i uses base scrollx + m_other[(i + otheroffs) & 0x3ff]).
            let row_scrollx = match rowscroll {
                Some((rs_base, offs)) => {
                    let idx = ((sy as i32 + SCREEN_Y_OFF + offs) & 0x3ff) as usize;
                    scrollx + bus.gfxram.get(rs_base + idx).copied().unwrap_or(0) as i32
                }
                None => scrollx,
            };
            for sx in 0..WIDTH {
                let wx = sx as i32 + SCREEN_X_OFF + row_scrollx;
                let col = wx.div_euclid(size as i32);
                let tx = (wx & mask) as usize;
                let idx = tilemap_scan(size, row, col);
                let entry = base + idx * 2;
                let code = bus.gfxram.get(entry).copied().unwrap_or(0) & code_mask;
                let attr = bus.gfxram.get(entry + 1).copied().unwrap_or(0);
                let flipx = attr & 0x20 != 0;
                let flipy = attr & 0x40 != 0;
                let fx = if flipx { size - 1 - tx } else { tx };
                let fy = if flipy { size - 1 - ty } else { ty };
                let phys = bus.map_gfx(gfxtype, code as i32);
                let phys = if phys < 0 { code as i32 } else { phys };
                // 8x8 tiles pick the left/right 8 pixels of the 16-pixel fetch
                // based on the screen column parity (MAME: gfxset = BIT(index,5)).
                let gfxset = if size == 8 { (col & 1) as usize } else { 0 };
                let pen = self.tile_pixel(size, phys, fx, fy, gfxset);
                if pen == 15 {
                    continue;
                }
                let group = ((attr >> 7) & 3) as usize;
                let high = (prio_reg >> pen) & 1;
                let _ = group; // prio_reg already selected per group by caller
                let color = color_add + (attr & 0x1f);
                let palidx = color * 16 + pen as u16;
                let level = if high != 0 { HIGH_LEVEL } else { slot_level };
                self.put(sx, sy, palidx, level);
            }
        }
    }

    fn draw_scroll1(&mut self, bus: &CpsBus, level: u8) {
        let base = bus.video_base(SCROLL1_BASE, SCROLL_SIZE);
        let sx = bus.cps_a[SCROLL1_X] as i16 as i32;
        let sy = bus.cps_a[SCROLL1_Y] as i16 as i32;
        let prio = self.prio_reg(bus, 0);
        // MAME get_tile0_info uses the full 16-bit code (no mask).
        self.draw_tilemap(bus, 8, base, sx, sy, 0x20, GFXTYPE_SCROLL1, 0xffff, prio, level, None);
    }

    fn draw_scroll2(&mut self, bus: &CpsBus, level: u8) {
        let base = bus.video_base(SCROLL2_BASE, SCROLL_SIZE);
        let sx = bus.cps_a[SCROLL2_X] as i16 as i32;
        let sy = bus.cps_a[SCROLL2_Y] as i16 as i32;
        let prio = self.prio_reg(bus, 0);
        // Scroll2 line-scroll (MAME: videocontrol bit 0). Each screen row adds
        // m_other[(line + rowscroll_offs) & 0x3ff] to the base X. SF2 uses this
        // for its parallax backgrounds; without it the background is garbled.
        let rowscroll = if bus.cps_a[VIDEOCONTROL] & 0x01 != 0 {
            let rs_base = bus.video_base(OTHER_BASE, OTHER_SIZE);
            let offs = bus.cps_a[ROWSCROLL_OFFS] as i32;
            Some((rs_base, offs))
        } else {
            None
        };
        self.draw_tilemap(bus, 16, base, sx, sy, 0x40, GFXTYPE_SCROLL2, 0xffff, prio, level, rowscroll);
    }

    fn draw_scroll3(&mut self, bus: &CpsBus, level: u8) {
        let base = bus.video_base(SCROLL3_BASE, SCROLL_SIZE);
        let sx = bus.cps_a[SCROLL3_X] as i16 as i32;
        let sy = bus.cps_a[SCROLL3_Y] as i16 as i32;
        let prio = self.prio_reg(bus, 0);
        self.draw_tilemap(bus, 32, base, sx, sy, 0x60, GFXTYPE_SCROLL3, 0x3fff, prio, level, None);
    }

    /// Read the CPS-B priority mask register for tile group 0 (SF2 uses the
    /// same mask across groups for the supported games).
    fn prio_reg(&self, bus: &CpsBus, group: usize) -> u16 {
        let p = bus.game().cpsb.priority[group];
        if p < 0 {
            0
        } else {
            bus.cps_b[((p / 2) as usize) & 0x1f]
        }
    }

    // ── Sprites ──────────────────────────────────────────────────────────────

    fn draw_sprites(&mut self, bus: &CpsBus, level: u8) {
        // Sprites are drawn from the one-frame-delayed OBJ snapshot latched by
        // `buffer_sprites` (mirrors MAME's `m_buffered_obj`), not the live list.
        let obj = &self.buffered_obj;
        let get = |i: usize| obj.get(i).copied().unwrap_or(0xff00);
        // Locate the end-of-list marker.
        let mut last = 0usize;
        for i in 0..256 {
            let terminator = get(i * 4 + 3);
            if terminator & 0xff00 == 0xff00 {
                break;
            }
            last = i;
        }
        // Collect sprite descriptors before drawing (draw_sprite_cell borrows
        // &mut self, which would conflict with the &self.buffered_obj slice).
        let mut sprites: Vec<(i32, u16, i32, i32, bool, bool)> = Vec::new();
        for i in (0..=last).rev() {
            let o = i * 4;
            let x0 = get(o) as i32 & 0x1ff;
            let y0 = get(o + 1) as i32 & 0x1ff;
            let code = get(o + 2) as i32;
            let colour = get(o + 3);
            let col = (colour & 0x1f) as u16;
            let flipx = colour & 0x20 != 0;
            let flipy = colour & 0x40 != 0;
            let nx = ((colour >> 8) & 0x0f) as i32 + 1;
            let ny = ((colour >> 12) & 0x0f) as i32 + 1;

            for bx in 0..nx {
                for by in 0..ny {
                    // Block sprite tile numbering (exact MAME cps1_render_sprites):
                    //   tile = (code & ~0xf) + ((code + xidx) & 0xf) + 0x10*yidx
                    // The Y block index scales by 0x10 arithmetically (it can
                    // carry past the low tile block); it must NOT be masked into
                    // the high nibble, or tall sprites (e.g. Ryu) fetch wrong
                    // tiles and visibly glitch.
                    let xidx = if flipx { nx - 1 - bx } else { bx };
                    let yidx = if flipy { ny - 1 - by } else { by };
                    let tile = (code & !0xf) + ((code + xidx) & 0x0f) + 0x10 * yidx;
                    let sx = ((x0 + bx * 16) & 0x1ff) - SCREEN_X_OFF;
                    let sy = ((y0 + by * 16) & 0x1ff) - SCREEN_Y_OFF;
                    sprites.push((tile, col, sx, sy, flipx, flipy));
                }
            }
        }
        for (tile, col, sx, sy, flipx, flipy) in sprites {
            self.draw_sprite_cell(bus, tile, col, sx, sy, flipx, flipy, level);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_sprite_cell(
        &mut self,
        bus: &CpsBus,
        code: i32,
        col: u16,
        sx: i32,
        sy: i32,
        flipx: bool,
        flipy: bool,
        level: u8,
    ) {
        let phys = bus.map_gfx(GFXTYPE_SPRITES, code);
        let phys = if phys < 0 { code } else { phys };
        for py in 0..16 {
            let yy = sy + py;
            if yy < 0 || yy >= HEIGHT as i32 {
                continue;
            }
            let fy = if flipy { 15 - py } else { py } as usize;
            for px in 0..16 {
                let xx = sx + px;
                if xx < 0 || xx >= WIDTH as i32 {
                    continue;
                }
                let fx = if flipx { 15 - px } else { px } as usize;
                let pen = self.tile_pixel(16, phys, fx, fy, 0);
                if pen == 15 {
                    continue;
                }
                let palidx = col * 16 + pen as u16;
                self.put(xx as usize, yy as usize, palidx, level);
            }
        }
    }

    // ── Frame composition ────────────────────────────────────────────────────

    pub fn render(&mut self, bus: &CpsBus) {
        self.build_palette(bus);
        // MAME cps1_v.cpp screen_update: blank the screen with pen 0xbff
        // (the CPS1 backdrop/border colour) before drawing the layers.
        for v in self.color.iter_mut() {
            *v = 0xbff;
        }
        for v in self.pri.iter_mut() {
            *v = 0;
        }

        let lc = bus.cps_b[((bus.game().cpsb.layer_control / 2) as usize) & 0x1f];
        let masks = &bus.game().cpsb.layer_enable_mask;
        // MAME cps1_get_video_base(): scroll2/scroll3 are additionally gated by
        // videocontrol bits 2/3 (boot/attract screens clear these to hide the
        // uninitialised background layers).
        let videocontrol = bus.cps_a[VIDEOCONTROL];
        let enable_scroll1 = lc & masks[0] as u16 != 0;
        let enable_scroll2 = lc & masks[1] as u16 != 0 && videocontrol & 0x04 != 0;
        let enable_scroll3 = lc & masks[2] as u16 != 0 && videocontrol & 0x08 != 0;

        // Draw order (bottom → top); each 2-bit field selects a layer.
        // 0 = sprites, 1 = scroll1, 2 = scroll2, 3 = scroll3.
        for slot in 0..4u8 {
            let which = (lc >> (6 + slot * 2)) & 3;
            let level = (slot + 1) * 2;
            match which {
                0 => self.draw_sprites(bus, level),
                1 => {
                    if enable_scroll1 {
                        self.draw_scroll1(bus, level);
                    }
                }
                2 => {
                    if enable_scroll2 {
                        self.draw_scroll2(bus, level);
                    }
                }
                3 => {
                    if enable_scroll3 {
                        self.draw_scroll3(bus, level);
                    }
                }
                _ => {}
            }
        }

        // Resolve palette indices to RGB24.
        // NOTE: the colour index (colour*16 + pen, up to 0x7ff, plus the 0xbff
        // backdrop) must NOT be masked with `& 0xbff` — 0xbff clears bit 10
        // (0x400), which folds scroll2 (page 2) onto the sprite palette and
        // scroll3 (page 3) onto the scroll1 palette, wrecking their colours.
        // Clamp instead so out-of-range indices stay in bounds.
        for i in 0..WIDTH * HEIGHT {
            let (r, g, b) = self.palette[(self.color[i] as usize).min(0xbff)];
            let o = i * 3;
            self.framebuffer[o] = r;
            self.framebuffer[o + 1] = g;
            self.framebuffer[o + 2] = b;
        }
    }
}

/// Per-layer tilemap scan (row/col → linear tile index), port of MAME
/// `tilemapN_scan`.
#[inline]
fn tilemap_scan(size: usize, row: i32, col: i32) -> usize {
    match size {
        8 => ((row & 0x1f) + ((col & 0x3f) << 5) + ((row & 0x20) << 6)) as usize,
        16 => ((row & 0x0f) + ((col & 0x3f) << 4) + ((row & 0x30) << 6)) as usize,
        _ => ((row & 0x07) + ((col & 0x3f) << 3) + ((row & 0x38) << 6)) as usize,
    }
}
