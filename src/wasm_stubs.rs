#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn neo_ym2610_read(_addr: u16) -> u8 {
    0
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn neo_ym2610_write(_addr: u16, _data: u8) {}
