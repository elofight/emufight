//! Platform I/O traits — decouple the emulator core from the host backend.
//!
//! Host backends implement the traits below. The emulator core only depends
//! on these abstractions (or the simpler frame API on [`crate::EmulatorCore`]).

// ── Input ─────────────────────────────────────────────────────────────────────

/// One frame of player input.  All bytes are **active-low**: a cleared bit
/// means the button/switch is pressed.
///
/// Wire mapping matches the NeoGeo hardware registers directly:
///
/// | field  | register    | bits                                             |
/// |--------|-------------|--------------------------------------------------|
/// | `p1`   | REG_P1CNT   | 0=Up 1=Down 2=Left 3=Right 4=A 5=B 6=C 7=D      |
/// | `p2`   | REG_P2CNT   | same as p1                                       |
/// | `sys`  | REG_STATUS_B| 0=P1-Start 1=P1-Sel 2=P2-Start 3=P2-Sel         |
/// |        |             | 4=memcard-in(0=in) 5=WP(1=writable)             |
/// |        |             | 7=MVS(1)/AES(0)                                 |
/// | `coin` | REG_STATUS_A| bits\[5:0\] = coin slots 1–4 (active low); 4–7 = 1|
///
/// Hardware DIPs (`REG_DIPSW`) are not carried in `InputState`; the bus
/// exposes them via `SystemBus::effective_hw_dips` (training boots latch
/// freeplay per FBNeo `neoForceAES` defaults).
/// |        |             | bit 2 = Service/Test (0=active); also mirrored  |
/// |        |             | to REG_DIPSW ($300001) bit 2 by `apply_input`   |
///
/// `ext` carries the two extra face buttons (E/F) for 6-button games such as
/// the CPS1 Street Fighter II series (active-low, bit 0=P1E, 1=P1F, 2=P2E,
/// 3=P2F).  NeoGeo cores ignore it.
///
/// The packed netplay / replay wire format (see `pack_input`) is **per-peer**:
/// each element describes a single player, so its `p2` byte is always unused
/// (`0xFF`) — the combine step reconstructs the second player from the *other*
/// peer's `p1`.  That otherwise-wasted byte carries the peer's own `ext` nibble
/// instead, so 6-button games are fully supported online without changing the
/// fixed 32-bit wire width.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InputState {
    pub p1:   u8,
    pub p2:   u8,
    pub sys:  u8,
    pub coin: u8,
    pub ext:  u8,
}

impl Default for InputState {
    /// "No buttons pressed" — MVS idle (bit 7 high), all coin slots idle.
    fn default() -> Self {
        Self { p1: 0xFF, p2: 0xFF, sys: 0xFF, coin: 0x3F, ext: 0x0F }
    }
}

pub type PackedInput = u32;

/// Raw active-low packing of the idle `InputState::default` (p1=0xFF, ext=0x0F,
/// sys=0xFF, coin=0x3F).  The wire format XORs every packed value with this
/// constant so that **"nothing pressed" encodes to all-zero bits**.
///
/// This is required for GGRS rollback: GGRS synthesises blank inputs with
/// `bytemuck::Zeroable::zeroed()` (an all-zero `u32`) for the initial
/// `input_delay` padding frames and for prediction placeholders.  With the raw
/// active-low layout those zero bits would decode to *every* button + Start +
/// coin pressed, which instantly confirms the char-select screen and inserts
/// phantom coins on the first frames of an online match.  Anchoring idle at
/// zero makes the blank frames genuinely idle.
const PACK_IDLE_XOR: u32 = 0x3FFF_0FFF;

pub fn pack_input(input: &InputState) -> PackedInput {
    // Pack a single peer's input into a u32.  Must match unpack_input.
    //
    // Byte 1 carries `ext` (6-button E/F) rather than `p2`: on the wire every
    // packed value is per-peer, so `p2` is always the unused 0xFF placeholder
    // (the combine step derives the second player from the other peer's `p1`).
    // Reusing that byte keeps the wire/replay/FFI format a fixed 32 bits while
    // still transmitting the extra buttons.
    //
    // The final XOR anchors the idle state at zero (see [`PACK_IDLE_XOR`]).
    let raw = ((input.p1 as u32) << 0)
        | ((input.ext as u32) << 8)
        | ((input.sys as u32) << 16)
        | ((input.coin as u32) << 24);
    raw ^ PACK_IDLE_XOR
}

pub fn unpack_input(packed: PackedInput) -> InputState {
    let raw = packed ^ PACK_IDLE_XOR;
    InputState {
        p1:   ((raw >> 0) & 0xff) as u8,
        // p2 is not carried per-peer; the combine step supplies the real value
        // from the other peer's p1.  Default to the "released" placeholder.
        p2:   0xFF,
        sys:  ((raw >> 16) & 0xff) as u8,
        coin: ((raw >> 24) & 0xff) as u8,
        ext:  ((raw >> 8) & 0xff) as u8,
    }
}

/// Merge two per-peer `ext` nibbles (each carrying that peer's own E/F in bits
/// 0–1) into the combined `InputState::ext` layout (P1 E/F in bits 0–1, P2 E/F
/// in bits 2–3).  All bits are active-low, so "nothing pressed" round-trips to
/// `0x0F`.
pub fn combine_ext(p0_ext: u8, p1_ext: u8) -> u8 {
    (p0_ext & 0x03) | ((p1_ext & 0x03) << 2)
}

// ── Output traits ─────────────────────────────────────────────────────────────

/// Receives one rendered RGB24 frame (`SCREEN_W × SCREEN_H × 3` bytes).
pub trait VideoSink {
    fn present(&mut self, frame: &[u8]);
}

/// Receives f32 mono PCM samples and reports queue depth for adaptive throttling.
pub trait AudioSink {
    /// Append `samples` to the output queue.
    fn queue(&mut self, samples: &[f32]);
}

// ── Input trait ───────────────────────────────────────────────────────────────

/// Polls OS / hardware events and returns the current input state.
pub trait InputSource {
    /// Process pending events and return `(inputs, quit_requested)`.
    fn poll(&mut self) -> (InputState, bool);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_round_trips_per_peer_fields() {
        // A single peer's input: joystick+buttons in p1, own E/F in ext bits 0–1.
        let peer = InputState {
            p1: 0b1010_0101,
            p2: 0xFF, // unused on the wire
            sys: 0b1111_1010,
            coin: 0b0011_1110,
            ext: 0b0000_1101, // P1F released? bit1=0 pressed; carries own E/F
        };
        let back = unpack_input(pack_input(&peer));
        assert_eq!(back.p1, peer.p1);
        assert_eq!(back.sys, peer.sys);
        assert_eq!(back.coin, peer.coin);
        assert_eq!(back.ext, peer.ext, "ext must survive the wire round-trip");
        assert_eq!(back.p2, 0xFF, "p2 is not carried per-peer");
    }

    #[test]
    fn idle_input_encodes_to_zero_for_ggrs_blank_frames() {
        // GGRS synthesises blank/prediction inputs as an all-zero u32
        // (`bytemuck::Zeroable::zeroed()`).  The wire format must therefore make
        // "nothing pressed" encode to zero, otherwise the initial input_delay
        // padding frames would apply every button + Start + coin at once.
        assert_eq!(pack_input(&InputState::default()), 0,
            "idle input must pack to zero");

        let blank = unpack_input(0);
        assert_eq!(blank.p1, 0xFF, "blank frame: no P1 buttons pressed");
        assert_eq!(blank.sys, 0xFF, "blank frame: no Start/Select pressed");
        assert_eq!(blank.coin, 0x3F, "blank frame: no coin inserted");
        assert_eq!(blank.ext, 0x0F, "blank frame: no E/F pressed");
    }

    #[test]
    fn combine_ext_maps_each_peer_to_correct_player() {
        // All released.
        assert_eq!(combine_ext(0x0F, 0x0F), 0x0F);

        // Peer 0 presses E (bit0=0) → combined P1E (bit0=0).
        assert_eq!(combine_ext(0b1110, 0x0F) & 0x01, 0x00);
        // Peer 1 presses F (bit1=0) → combined P2F (bit3=0).
        assert_eq!(combine_ext(0x0F, 0b1101) & 0x08, 0x00);
        // Peer 0's buttons never leak into the P2 nibble.
        assert_eq!(combine_ext(0b1100, 0x0F) & 0x0C, 0x0C);
    }
}

