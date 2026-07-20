//! YM2610 (Yamaha OPNB) sound chip — FFI bridge to the ymfm C++ LLE core.
//!
//! `build.rs` compiles `ym2610_glue.cpp` into the crate.  The [`YM2610`] struct
//! wraps C functions via `extern "C"`.  [`AudioController`] clocks the chip
//! each batch and accumulates f32 mono PCM samples at 44.1 kHz.
//!
//! The YM2610 /INT line is routed to each emulator's Z80 via a bound pointer
//! (`neo_ym2610_bind_z80_int`).

#[cfg(not(target_arch = "wasm32"))]
extern "C" {
    fn neo_ym2610_create() -> *mut std::ffi::c_void;
    fn neo_ym2610_destroy(ptr: *mut std::ffi::c_void);
    fn neo_ym2610_reset(ptr: *mut std::ffi::c_void);
    fn neo_ym2610_write(ptr: *mut std::ffi::c_void, port: i32, data: i32);
    fn neo_ym2610_read(ptr: *mut std::ffi::c_void, port: i32) -> i32;
    fn neo_ym2610_generate(ptr: *mut std::ffi::c_void, buf: *mut f32, n: i32);
    fn neo_ym2610_ring_count(ptr: *mut std::ffi::c_void) -> i32;
    fn neo_ym2610_tick(ptr: *mut std::ffi::c_void, m68k_cycles: i32);
    fn neo_ym2610_begin_frame(ptr: *mut std::ffi::c_void);
    fn neo_ym2610_bind_z80_int(ptr: *mut std::ffi::c_void, z80_int: *mut i32);
    fn neo_ym2610_load_adpcm_a(ptr: *mut std::ffi::c_void, data: *const u8, size: i32);
    fn neo_ym2610_load_adpcm_b(ptr: *mut std::ffi::c_void, data: *const u8, size: i32);
    fn neo_ym2610_save_state(ptr: *mut std::ffi::c_void, out_buf: *mut *mut u8, out_size: *mut i32);
    fn neo_ym2610_load_state(ptr: *mut std::ffi::c_void, buf: *const u8, size: i32);
    fn neo_ym2610_free_state_buf(buf: *mut u8);
    fn neo_ym2610_get_irq_assert_count(ptr: *mut std::ffi::c_void) -> u64;
    fn neo_ym2610_get_timer_expire_count(ptr: *mut std::ffi::c_void, timer_idx: i32) -> u64;
}

#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_create() -> *mut std::ffi::c_void { std::ptr::null_mut() }
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_destroy(_ptr: *mut std::ffi::c_void) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_reset(_ptr: *mut std::ffi::c_void) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_write(_ptr: *mut std::ffi::c_void, _port: i32, _data: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_read(_ptr: *mut std::ffi::c_void, _port: i32) -> i32 { 0 }
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_generate(_ptr: *mut std::ffi::c_void, _buf: *mut f32, _n: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_ring_count(_ptr: *mut std::ffi::c_void) -> i32 { 0 }
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_tick(_ptr: *mut std::ffi::c_void, _m68k_cycles: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_begin_frame(_ptr: *mut std::ffi::c_void) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_bind_z80_int(_ptr: *mut std::ffi::c_void, _z80_int: *mut i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_load_adpcm_a(_ptr: *mut std::ffi::c_void, _data: *const u8, _size: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_load_adpcm_b(_ptr: *mut std::ffi::c_void, _data: *const u8, _size: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_save_state(_ptr: *mut std::ffi::c_void, _out_buf: *mut *mut u8, _out_size: *mut i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_load_state(_ptr: *mut std::ffi::c_void, _buf: *const u8, _size: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_free_state_buf(_buf: *mut u8) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_get_irq_assert_count(_ptr: *mut std::ffi::c_void) -> u64 { 0 }
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2610_get_timer_expire_count(_ptr: *mut std::ffi::c_void, _timer_idx: i32) -> u64 { 0 }

pub struct YM2610 {
    ptr: *mut std::ffi::c_void,
}

// Send: chip is owned exclusively after construction; Drop destroys C++ state.
// Not Sync: IRQ binding and C++ chip state are not thread-safe for shared access.
unsafe impl Send for YM2610 {}

impl Drop for YM2610 {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { neo_ym2610_destroy(self.ptr); }
            self.ptr = std::ptr::null_mut();
        }
    }
}

impl YM2610 {
    pub fn new() -> Self {
        Self {
            ptr: unsafe { neo_ym2610_create() },
        }
    }

    pub fn ptr(&self) -> *mut std::ffi::c_void {
        self.ptr
    }

    pub fn reset(&mut self) {
        unsafe { neo_ym2610_reset(self.ptr); }
    }

    pub fn write(&mut self, port: u8, data: u8) {
        unsafe { neo_ym2610_write(self.ptr, port as i32, data as i32); }
    }

    pub fn read(&mut self, port: u8) -> u8 {
        unsafe { neo_ym2610_read(self.ptr, port as i32) as u8 }
    }

    /// Generate `n` mono float samples into `buf` (already resized by caller).
    pub fn generate(&mut self, buf: &mut [f32]) {
        if buf.is_empty() { return; }
        unsafe { neo_ym2610_generate(self.ptr, buf.as_mut_ptr(), buf.len() as i32); }
    }

    /// Samples waiting in the glue ring (not yet passed to `generate`).
    pub fn ring_count(&self) -> usize {
        unsafe { neo_ym2610_ring_count(self.ptr).max(0) as usize }
    }

    /// Align ymfm IRQ edge tracking with the Z80 INT line at a frame boundary.
    pub fn begin_frame(&self) {
        unsafe { neo_ym2610_begin_frame(self.ptr); }
    }

    /// Bind this chip's IRQ output to an emulator-owned Z80 /INT cell.
    pub fn bind_z80_int(&mut self, z80_int: *mut i32) {
        unsafe { neo_ym2610_bind_z80_int(self.ptr, z80_int); }
    }

    pub fn update(&mut self, cycles: usize) {
        // Drive the chip per-batch so timer IRQs fire interleaved with Z80
        // execution at their true rate (not batched once per frame).
        if cycles > 0 {
            unsafe { neo_ym2610_tick(self.ptr, cycles as i32); }
        }
    }

    /// Load ADPCM-A sample ROM (NeoGeo v1+v2 concatenated).
    pub fn load_adpcm_a(&mut self, data: &[u8]) {
        if data.is_empty() { return; }
        unsafe { neo_ym2610_load_adpcm_a(self.ptr, data.as_ptr(), data.len() as i32); }
    }

    /// Load ADPCM-B sample ROM (NeoGeo v3+v4 concatenated).
    pub fn load_adpcm_b(&mut self, data: &[u8]) {
        if data.is_empty() { return; }
        unsafe { neo_ym2610_load_adpcm_b(self.ptr, data.as_ptr(), data.len() as i32); }
    }

    /// Capture the full ymfm chip register state for rollback / save states.
    pub fn snapshot(&self) -> Vec<u8> {
        if self.ptr.is_null() {
            return Vec::new();
        }
        let mut buf: *mut u8 = std::ptr::null_mut();
        let mut size: i32 = 0;
        unsafe {
            neo_ym2610_save_state(self.ptr, &mut buf, &mut size);
            if buf.is_null() || size <= 0 {
                return Vec::new();
            }
            let bytes = std::slice::from_raw_parts(buf, size as usize).to_vec();
            neo_ym2610_free_state_buf(buf);
            bytes
        }
    }

    /// Restore ymfm chip register state from a blob produced by `snapshot`.
    pub fn restore(&mut self, data: &[u8]) {
        if data.is_empty() { return; }
        // Reset glue/chip to power-on defaults before deserialising so a
        // long-lived instance matches a fresh emulator after the same blob.
        self.reset();
        self.load_snapshot(data);
    }

    /// Deserialize chip state into a freshly constructed instance (no reset).
    pub fn load_snapshot(&mut self, data: &[u8]) {
        if data.is_empty() { return; }
        unsafe { neo_ym2610_load_state(self.ptr, data.as_ptr(), data.len() as i32); }
    }

    pub fn irq_assert_count(&self) -> u64 {
        unsafe { neo_ym2610_get_irq_assert_count(self.ptr) }
    }

    pub fn timer_expire_count(&self, timer_idx: usize) -> u64 {
        unsafe { neo_ym2610_get_timer_expire_count(self.ptr, timer_idx as i32) }
    }
}

// Audio Controller — clocks the YM2610 and accumulates mono f32 PCM.

pub struct AudioController {
    pub sample_count: u64,
}

impl AudioController {
    pub fn new() -> Self {
        Self { sample_count: 0 }
    }

    /// Generate `n_samples` from the YM2610 LLE and append them to `buffer`.
    pub fn generate_samples(
        &mut self,
        buffer:   &mut Vec<f32>,
        ym2610:   &mut YM2610,
        n_samples: usize,
    ) {
        let start = buffer.len();
        buffer.resize(start + n_samples, 0.0f32);
        ym2610.generate(&mut buffer[start..]);
        self.sample_count += n_samples as u64;
    }
}
