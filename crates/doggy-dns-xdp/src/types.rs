/// Must match xdp/doggy-dns-xdp-ebpf/src/main.rs exactly.
pub const FLAG_NEGATIVE: u16 = 0x01;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CacheKey {
    pub name_hash: u64,
    pub qtype_class: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CacheValue {
    pub resp_data: [u8; 512],
    pub resp_len: u16,
    pub flags: u16,
    pub _pad: u32,
    pub expires_at: u64,
}

impl CacheValue {
    pub fn new_positive(resp_data: Vec<u8>, expires_at: u64) -> Self {
        let len = resp_data.len().min(512);
        let mut data = [0u8; 512];
        data[..len].copy_from_slice(&resp_data[..len]);
        Self {
            resp_data: data,
            resp_len: len as u16,
            flags: 0,
            _pad: 0,
            expires_at,
        }
    }

    pub fn new_negative(resp_data: Vec<u8>, expires_at: u64) -> Self {
        let len = resp_data.len().min(512);
        let mut data = [0u8; 512];
        data[..len].copy_from_slice(&resp_data[..len]);
        Self {
            resp_data: data,
            resp_len: len as u16,
            flags: FLAG_NEGATIVE,
            _pad: 0,
            expires_at,
        }
    }

    pub fn is_negative(&self) -> bool {
        self.flags & FLAG_NEGATIVE != 0
    }
}

// Safety: CacheKey and CacheValue are #[repr(C)] with only Pod-compatible types.
// These assertions verify layout at compile time.
const _: () = {
    assert!(core::mem::size_of::<CacheKey>() == 16);
    assert!(core::mem::size_of::<CacheValue>() == 528);
};
