//! `ozan-shm` — `ozan.exe` ile `audiodg.exe` içindeki `ozan-apo.dll` arasında
//! Windows Named Shared Memory ve kilitsiz (lock-free) SPSC halka tampon protokolü.
//!
//! Gerçek zamanlı `APOProcess` döngüsünde sıfır bellek ayırma (zero heap allocation),
//! sıfır kilit (no Mutex) ve sıfır disk erişimi garantisiyle çalışır.

use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    InitializeSecurityDescriptor, SetSecurityDescriptorDacl, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

/// Ozan User-Mode APO COM Sınıf Kimliği (CLSID).
pub const OZAN_APO_CLSID_STR: &str = "{8A4E7B21-6C3D-4F92-B8E1-5D2A9F0C4E73}";

pub const OZAN_APO_CLSID: GUID = GUID {
    data1: 0x8A4E_7B21,
    data2: 0x6C3D,
    data3: 0x4F92,
    data4: [0xB8, 0xE1, 0x5D, 0x2A, 0x9F, 0x0C, 0x4E, 0x73],
};

/// Windows Named Shared Memory isimleri.
/// Önce oturumlar arası (`audiodg.exe` Session 0 <-> Kullanıcı Session 1+) `Global\` denenir,
/// yetki kısıtı varsa `Local\` isim alanına düşülür.
pub const SHM_NAME_GLOBAL: &str = "Global\\OzanAudioSharedMem_v1";
pub const SHM_NAME_LOCAL: &str = "Local\\OzanAudioSharedMem_v1";

pub const SHM_MAGIC: u32 = 0x4F5A_414E; // "OZAN"
pub const SHM_VERSION: u32 = 1;

/// Halka tampon kapasitesi (2'nin kuvveti olmalıdır: 65536 örnek ≈ 48kHz stereo'da ~682 ms).
pub const SHM_RING_CAPACITY: usize = 65_536;
pub const SHM_RING_MASK: usize = SHM_RING_CAPACITY - 1;

/// Durum bayrakları (`OzanSharedBlock::flags`).
pub const FLAG_PRODUCER_ACTIVE: u32 = 1 << 0;
pub const FLAG_APO_ACTIVE: u32 = 1 << 1;
pub const FLAG_MUTE_PHYSICAL_MIC: u32 = 1 << 2;
pub const FLAG_LOCAL_PREVIEW_ONLY: u32 = 1 << 3;
pub const FLAG_CHAINING_ACTIVE: u32 = 1 << 4;

/// Ses örnek biçimi etiketleri (`apo_format`).
pub const APO_FORMAT_F32: u32 = 1;
pub const APO_FORMAT_I16: u32 = 2;

/// Sahte paylaşımı (false sharing) önlemek için önbellek satırı (64 bayt) hizalı atomik sayaç.
#[repr(C, align(64))]
pub struct CacheAlignedAtomicUsize(pub AtomicUsize);

/// Paylaşımlı bellek bloğu (`#[repr(C, align(64))]`).
#[repr(C, align(64))]
pub struct OzanSharedBlock {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub flags: AtomicU32,
    /// `ozan-apo.dll` tarafından `LockForProcess` sırasında bildirilen mikrofon örnekleme hızı (Hz).
    pub apo_sample_rate: AtomicU32,
    /// `ozan-apo.dll` tarafından bildirilen mikrofon kanal sayısı (1 = Mono, 2 = Stereo vb.).
    pub apo_channels: AtomicU32,
    /// `APO_FORMAT_F32` veya `APO_FORMAT_I16`.
    pub apo_format: AtomicU32,
    /// Enjekte edilen sesin ana kazanç çarpanı (`f32::to_bits`).
    pub inject_gain_bits: AtomicU32,
    /// Zincirlenen (chained) orijinal üretici APO sayısı.
    pub chained_apo_count: AtomicU32,
    /// `ozan-apo.dll` tarafından her `APOProcess` çağrısında artırılan canlılık sayacı.
    pub apo_heartbeat: AtomicU64,
    /// `ozan.exe` tarafından her ses paketi yazımında artırılan canlılık sayacı.
    pub producer_heartbeat: AtomicU64,
    /// Orijinal üretici APO CLSID değeri (UTF-16, sonu sıfır karakterli, ör. `"{xxxxxxxx-...}"`).
    pub chained_clsid_utf16: [AtomicU16; 40],

    /// Üretici (`ozan.exe`) yazma indeksi — ayrı önbellek satırında.
    pub write_pos: CacheAlignedAtomicUsize,
    /// Tüketici (`ozan-apo.dll`) okuma indeksi — ayrı önbellek satırında.
    pub read_pos: CacheAlignedAtomicUsize,

    /// Kilitsiz SPSC halka tamponu (`f32::to_bits` olarak saklanır).
    pub samples: [AtomicU32; SHM_RING_CAPACITY],
}

impl OzanSharedBlock {
    /// Paylaşımlı bellek ilk kez oluşturulduğunda başlık alanlarını ilklendirir.
    #[inline]
    pub fn init_if_needed(&self) {
        if self.magic.load(Ordering::Acquire) != SHM_MAGIC {
            self.version.store(SHM_VERSION, Ordering::Relaxed);
            self.flags.store(0, Ordering::Relaxed);
            self.apo_sample_rate.store(48_000, Ordering::Relaxed);
            self.apo_channels.store(2, Ordering::Relaxed);
            self.apo_format.store(APO_FORMAT_F32, Ordering::Relaxed);
            self.inject_gain_bits.store(1.0f32.to_bits(), Ordering::Relaxed);
            self.chained_apo_count.store(0, Ordering::Relaxed);
            self.apo_heartbeat.store(0, Ordering::Relaxed);
            self.producer_heartbeat.store(0, Ordering::Relaxed);
            self.write_pos.0.store(0, Ordering::Relaxed);
            self.read_pos.0.store(0, Ordering::Relaxed);
            self.magic.store(SHM_MAGIC, Ordering::Release);
        }
    }

    /// Halka tampondaki okunmaya hazır örnek (`f32`) sayısını döndürür.
    #[inline(always)]
    pub fn available_read(&self) -> usize {
        let w = self.write_pos.0.load(Ordering::Acquire);
        let r = self.read_pos.0.load(Ordering::Relaxed);
        w.wrapping_sub(r).min(SHM_RING_CAPACITY)
    }

    /// Halka tamponda yazılabilir boş örnek (`f32`) sayısını döndürür.
    #[inline(always)]
    pub fn available_write(&self) -> usize {
        let w = self.write_pos.0.load(Ordering::Relaxed);
        let r = self.read_pos.0.load(Ordering::Acquire);
        SHM_RING_CAPACITY.saturating_sub(w.wrapping_sub(r))
    }

    /// Üretici (`ozan.exe`): Tampona `src` örneklerini kilitsiz yazar.
    /// Yazılan örnek sayısını döndürür.
    #[inline]
    pub fn push_samples(&self, src: &[f32]) -> usize {
        let w = self.write_pos.0.load(Ordering::Relaxed);
        let r = self.read_pos.0.load(Ordering::Acquire);
        let used = w.wrapping_sub(r);
        let free = SHM_RING_CAPACITY.saturating_sub(used);
        let n = src.len().min(free);
        if n == 0 {
            return 0;
        }
        for (i, &sample) in src[..n].iter().enumerate() {
            let idx = w.wrapping_add(i) & SHM_RING_MASK;
            self.samples[idx].store(sample.to_bits(), Ordering::Relaxed);
        }
        self.write_pos.0.store(w.wrapping_add(n), Ordering::Release);
        self.producer_heartbeat.fetch_add(1, Ordering::Relaxed);
        n
    }

    /// Tüketici (`ozan-apo.dll`): Tampondan en fazla `dst.len()` örneği kilitsiz ve
    /// sıfır bellek ayırmayla okur. Okunan örnek sayısını döndürür.
    #[inline(always)]
    pub fn pop_samples(&self, dst: &mut [f32]) -> usize {
        let r = self.read_pos.0.load(Ordering::Relaxed);
        let w = self.write_pos.0.load(Ordering::Acquire);
        let avail = w.wrapping_sub(r).min(SHM_RING_CAPACITY);
        let n = dst.len().min(avail);
        if n == 0 {
            return 0;
        }
        for (i, out) in dst[..n].iter_mut().enumerate() {
            let idx = r.wrapping_add(i) & SHM_RING_MASK;
            *out = f32::from_bits(self.samples[idx].load(Ordering::Relaxed));
        }
        self.read_pos.0.store(r.wrapping_add(n), Ordering::Release);
        n
    }

    /// Tüketici (`ozan-apo.dll`): Doğrudan ham çıkış işaretçisine (`*mut f32`) kilitsiz
    /// olarak örnekleri toplar (mix) ve `[-1.0, 1.0]` aralığında sınırlar.
    /// Hiçbir ara yığın/heap tamponu kullanmaz.
    ///
    /// # Safety
    /// `out_ptr` en az `sample_count` adet geçerli `f32` elemanına işaret etmelidir.
    #[inline(always)]
    pub unsafe fn mix_into_raw(&self, out_ptr: *mut f32, sample_count: usize, gain: f32) -> usize {
        let r = self.read_pos.0.load(Ordering::Relaxed);
        let w = self.write_pos.0.load(Ordering::Acquire);
        let avail = w.wrapping_sub(r).min(SHM_RING_CAPACITY);
        let n = sample_count.min(avail);
        if n == 0 {
            return 0;
        }
        for i in 0..n {
            let idx = r.wrapping_add(i) & SHM_RING_MASK;
            let inj = f32::from_bits(self.samples[idx].load(Ordering::Relaxed)) * gain;
            let p = out_ptr.add(i);
            *p = (*p + inj).clamp(-1.0, 1.0);
        }
        self.read_pos.0.store(r.wrapping_add(n), Ordering::Release);
        n
    }

    /// Tamponu anında temizler (Panic / StopAll durumunda).
    #[inline]
    pub fn clear(&self) {
        let w = self.write_pos.0.load(Ordering::Acquire);
        self.read_pos.0.store(w, Ordering::Release);
    }

    /// Zincirlenen orijinal üretici APO CLSID bilgisini yazar.
    pub fn set_chained_clsid(&self, clsid: Option<&str>) {
        for slot in &self.chained_clsid_utf16 {
            slot.store(0, Ordering::Relaxed);
        }
        if let Some(s) = clsid {
            let trimmed = s.trim();
            if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case(OZAN_APO_CLSID_STR) {
                for (i, ch) in trimmed.encode_utf16().take(39).enumerate() {
                    self.chained_clsid_utf16[i].store(ch, Ordering::Relaxed);
                }
                self.chained_apo_count.store(1, Ordering::Release);
                self.flags.fetch_or(FLAG_CHAINING_ACTIVE, Ordering::Release);
                return;
            }
        }
        self.chained_apo_count.store(0, Ordering::Release);
        self.flags.fetch_and(!FLAG_CHAINING_ACTIVE, Ordering::Release);
    }

    /// Zincirlenen orijinal üretici APO CLSID bilgisini sabit diziye okur (heap ayırmadan).
    #[inline]
    pub fn read_chained_clsid_buf(&self, out: &mut [u16; 40]) -> usize {
        let mut len = 0usize;
        for (i, slot) in self.chained_clsid_utf16.iter().enumerate() {
            let c = slot.load(Ordering::Relaxed);
            out[i] = c;
            if c == 0 {
                break;
            }
            len += 1;
        }
        out[39] = 0;
        len
    }
}

/// Windows Named Shared Memory görünümü (RAII sarmalayıcısı).
pub struct SharedMemoryMapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    is_global: bool,
}

unsafe impl Send for SharedMemoryMapping {}
unsafe impl Sync for SharedMemoryMapping {}

impl SharedMemoryMapping {
    /// Paylaşımlı belleği açar veya oluşturur (`Global\` öncelikli, `Local\` yedekli).
    pub fn create_or_open() -> Result<Self, String> {
        let size = size_of::<OzanSharedBlock>() as u32;

        // 1. Önce mevcut bir eşlemeyi açmayı dene (Global, sonra Local)
        for (name, is_global) in [(SHM_NAME_GLOBAL, true), (SHM_NAME_LOCAL, false)] {
            if let Some(m) = unsafe { Self::try_open(name, size, is_global) } {
                m.block().init_if_needed();
                return Ok(m);
            }
        }

        // 2. Yoksa NULL DACL güvenlik tanımlayıcısı ile oluştur (audiodg.exe erişimi için)
        for (name, is_global) in [(SHM_NAME_GLOBAL, true), (SHM_NAME_LOCAL, false)] {
            if let Ok(m) = unsafe { Self::try_create(name, size, is_global) } {
                m.block().init_if_needed();
                return Ok(m);
            }
        }

        Err("Paylaşımlı bellek (OzanAudioSharedMem_v1) oluşturulamadı veya açılamadı.".into())
    }

    unsafe fn try_open(name: &str, size: u32, is_global: bool) -> Option<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let h = OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide.as_ptr());
        if h.is_null() {
            return None;
        }
        let view = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, size as usize);
        if view.Value.is_null() {
            CloseHandle(h);
            return None;
        }
        Some(Self {
            handle: h,
            view,
            is_global,
        })
    }

    unsafe fn try_create(name: &str, size: u32, is_global: bool) -> Result<Self, String> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SECURITY_DESCRIPTOR için 64 baytlık hizalı tampon (x64'te 40 bayt)
        let mut sd_buf = [0u64; 8];
        let psd = sd_buf.as_mut_ptr() as PSECURITY_DESCRIPTOR;
        let has_sd = InitializeSecurityDescriptor(psd, 1) != 0
            && SetSecurityDescriptorDacl(psd, 1, null_mut(), 0) != 0;

        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: if has_sd { psd } else { null_mut() },
            bInheritHandle: 0,
        };

        let h = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            if has_sd { &sa } else { null_mut() },
            PAGE_READWRITE,
            0,
            size,
            wide.as_ptr(),
        );
        if h.is_null() {
            return Err(format!("{name} CreateFileMappingW başarısız oldu"));
        }
        let view = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, size as usize);
        if view.Value.is_null() {
            CloseHandle(h);
            return Err(format!("{name} MapViewOfFile başarısız oldu"));
        }
        Ok(Self {
            handle: h,
            view,
            is_global,
        })
    }

    #[inline(always)]
    pub fn block(&self) -> &OzanSharedBlock {
        unsafe { &*(self.view.Value as *const OzanSharedBlock) }
    }

    #[inline]
    pub fn namespace(&self) -> &'static str {
        if self.is_global {
            SHM_NAME_GLOBAL
        } else {
            SHM_NAME_LOCAL
        }
    }
}

impl Drop for SharedMemoryMapping {
    fn drop(&mut self) {
        unsafe {
            if !self.view.Value.is_null() {
                UnmapViewOfFile(self.view);
            }
            if !self.handle.is_null() {
                CloseHandle(self.handle);
            }
        }
    }
}
