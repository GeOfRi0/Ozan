//! `ozan-apo.dll` — Windows User-Mode Audio Processing Object (sAPO COM DLL).
//!
//! `audiodg.exe` (Windows Audio Device Graph Isolation) süreci içine yüklenir ve
//! aktif fiziksel mikrofonun yakalama (Capture) akışına `OzanAudioSharedMem_v1`
//! paylaşımlı belleği üzerinden gelen ses örneklerini gerçek zamanlı olarak enjekte eder.
//!
//! Temel Garantiler:
//! 1. **Sıfır Bellek Ayırma / Sıfır Kilit (`APOProcess`)**: Gerçek zamanlı ses işleme
//!    döngüsü içinde kesinlikle heap allocation (`Box`, `Vec`, `String`), kilit (`Mutex`)
//!    veya disk/kayıt defteri okuma işlemi yapılmaz.
//! 2. **APO Chaining (Zincirleme)**: Aktif mikrofonun mevcut üretici ses efektleri
//!    (gürültü engelleme, beamforming, AGC vb.) korunur; `OzanApo` ses verisini
//!    orijinal APO zincirinden geçirir ve enjekte edilen sesi karıştırır.
//! 3. **`rubato` ile Gerçek Zamanlı Yeniden Örnekleme ve Kanal Eşitleme**:
//!    44.1 kHz, 48 kHz, 96 kHz veya Bluetooth 16 kHz Mono gibi farklı mikrofon
//!    biçimlerine `LockForProcess` aşamasında ön-ayrılmış `rubato::FastFixedIn`
//!    tamponlarıyla gerçek zamanlı uyum sağlar.

#![allow(non_snake_case, non_camel_case_types, clippy::missing_safety_doc)]

use std::ffi::c_void;
use std::mem::{offset_of, size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU32, Ordering};

use ozan_shm::{
    OzanSharedBlock, SharedMemoryMapping, APO_FORMAT_F32, APO_FORMAT_I16, FLAG_APO_ACTIVE,
    FLAG_LOCAL_PREVIEW_ONLY, FLAG_MUTE_PHYSICAL_MIC, OZAN_APO_CLSID, OZAN_APO_CLSID_STR,
};
use rubato::{FastFixedIn, Indexing, PolynomialDegree, Resampler};
use windows_sys::core::{BOOL, GUID, HRESULT};
use windows_sys::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_INVALIDARG, E_NOINTERFACE,
    E_OUTOFMEMORY, E_POINTER, HINSTANCE, S_FALSE, S_OK, TRUE,
};
use windows_sys::Win32::System::Com::{
    CLSIDFromString, CoCreateInstance, CoTaskMemAlloc, CLSCTX_INPROC_SERVER,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

// ============================================================================
// COM Arayüz GUID Tanımları (Windows SDK audioenginebaseapo.h)
// ============================================================================

pub const IID_IUNKNOWN: GUID = GUID {
    data1: 0x0000_0000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

pub const IID_ICLASSFACTORY: GUID = GUID {
    data1: 0x0000_0001,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// `IAudioProcessingObject` `{FD7F2B29-24D0-4B5C-B177-592C39F9CA10}`
pub const IID_IAUDIOPROCESSINGOBJECT: GUID = GUID {
    data1: 0xFD7F_2B29,
    data2: 0x24D0,
    data3: 0x4B5C,
    data4: [0xB1, 0x77, 0x59, 0x2C, 0x39, 0xF9, 0xCA, 0x10],
};

/// `IAudioProcessingObjectRT` `{9E1D6A6D-DDBC-4E95-A4C7-AD64BA37846C}`
pub const IID_IAUDIOPROCESSINGOBJECT_RT: GUID = GUID {
    data1: 0x9E1D_6A6D,
    data2: 0xDDBC,
    data3: 0x4E95,
    data4: [0xA4, 0xC7, 0xAD, 0x64, 0xBA, 0x37, 0x84, 0x6C],
};

/// `IAudioProcessingObjectConfiguration` `{0E5ED805-ABA6-49C3-8F9A-2B8C889C4FA8}`
pub const IID_IAUDIOPROCESSINGOBJECT_CONFIG: GUID = GUID {
    data1: 0x0E5E_D805,
    data2: 0xABA6,
    data3: 0x49C3,
    data4: [0x8F, 0x9A, 0x2B, 0x8C, 0x88, 0x9C, 0x4F, 0xA8],
};

/// `IAudioSystemEffects` `{5FA00F27-ADD6-499A-8A9D-6B98521FA75B}`
pub const IID_IAUDIOSYSTEMEFFECTS: GUID = GUID {
    data1: 0x5FA0_0F27,
    data2: 0xADD6,
    data3: 0x499A,
    data4: [0x8A, 0x9D, 0x6B, 0x98, 0x52, 0x1F, 0xA7, 0x5B],
};

/// `IAudioSystemEffects2` `{BAFE99D2-7436-44CE-9E0E-4D89AFBFFF56}`
pub const IID_IAUDIOSYSTEMEFFECTS2: GUID = GUID {
    data1: 0xBAFE_99D2,
    data2: 0x7436,
    data3: 0x44CE,
    data4: [0x9E, 0x0E, 0x4D, 0x89, 0xAF, 0xBF, 0xFF, 0x56],
};

/// PCM / IEEE Float Medya Alt Türü GUID'leri
pub const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = GUID {
    data1: 0x0000_0003,
    data2: 0x0000,
    data3: 0x0010,
    data4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
};

pub const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID {
    data1: 0x0000_0001,
    data2: 0x0000,
    data3: 0x0010,
    data4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
};

// ============================================================================
// APO Yapıları ve Sabitleri
// ============================================================================

pub const APO_FLAG_NONE: u32 = 0x0000_0000;
pub const APO_FLAG_INPLACE: u32 = 0x0000_0001;
pub const APO_FLAG_SAMPLESPERFRAME_MUST_MATCH: u32 = 0x0000_0002;
pub const APO_FLAG_FRAMESPERSECOND_MUST_MATCH: u32 = 0x0000_0004;
pub const APO_FLAG_BITSPERSAMPLE_MUST_MATCH: u32 = 0x0000_0008;
pub const APO_FLAG_MIXER: u32 = 0x0000_0010;
pub const APO_FLAG_DEFAULT: u32 = APO_FLAG_INPLACE
    | APO_FLAG_SAMPLESPERFRAME_MUST_MATCH
    | APO_FLAG_FRAMESPERSECOND_MUST_MATCH
    | APO_FLAG_BITSPERSAMPLE_MUST_MATCH;

pub const BUFFER_INVALID: u32 = 0;
pub const BUFFER_VALID: u32 = 1;
pub const BUFFER_SILENT: u32 = 2;

/// `APO_CONNECTION_PROPERTY` (`audioenginebaseapo.h`)
#[repr(C)]
pub struct APO_CONNECTION_PROPERTY {
    pub pBuffer: usize,
    pub u32ValidFrameCount: u32,
    pub u32BufferFlags: u32,
    pub u32Signature: u32,
}

/// `UNCOMPRESSED_AUDIOMEDIATYPE` (`audiomediatype.h`)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UNCOMPRESSED_AUDIOMEDIATYPE {
    pub guidFormatType: GUID,
    pub dwSamplesPerFrame: u32,
    pub dwBytesPerSampleContainer: u32,
    pub dwValidBitsPerSample: u32,
    pub fFramesPerSecond: f32,
    pub dwChannelMask: u32,
}

/// `IAudioMediaType` VTable
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IAudioMediaTypeVtbl {
    pub QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub IsCompressedFormat: unsafe extern "system" fn(*mut c_void, *mut BOOL) -> HRESULT,
    pub IsEqual: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut u32) -> HRESULT,
    pub GetAudioFormat: unsafe extern "system" fn(*mut c_void) -> *const c_void,
    pub GetUncompressedAudioFormat:
        unsafe extern "system" fn(*mut c_void, *mut UNCOMPRESSED_AUDIOMEDIATYPE) -> HRESULT,
}

#[repr(C)]
pub struct IAudioMediaType {
    pub lpVtbl: *const IAudioMediaTypeVtbl,
}

/// `APO_CONNECTION_DESCRIPTOR`
#[repr(C)]
pub struct APO_CONNECTION_DESCRIPTOR {
    pub Type: i32,
    pub pBuffer: usize,
    pub u32MaxFrameCount: u32,
    pub pFormat: *mut IAudioMediaType,
    pub u32Signature: u32,
}

/// `APO_REG_PROPERTIES`
#[repr(C)]
pub struct APO_REG_PROPERTIES {
    pub clsid: GUID,
    pub Flags: u32,
    pub szFriendlyName: [u16; 256],
    pub szCopyrightInfo: [u16; 256],
    pub u32MajorVersion: u32,
    pub u32MinorVersion: u32,
    pub u32MinInputConnections: u32,
    pub u32MaxInputConnections: u32,
    pub u32MinOutputConnections: u32,
    pub u32MaxOutputConnections: u32,
    pub u32MaxInstances: u32,
    pub u32NumAPOInterfaces: u32,
    pub iidAPOInterfaceList: [GUID; 1],
}

// ============================================================================
// COM VTable Tanımları
// ============================================================================

#[repr(C)]
pub struct IUnknownVtbl {
    pub QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
pub struct IClassFactoryVtbl {
    pub QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub CreateInstance:
        unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub LockServer: unsafe extern "system" fn(*mut c_void, BOOL) -> HRESULT,
}

#[repr(C)]
pub struct IAudioProcessingObjectVtbl {
    pub QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Reset: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub GetLatency: unsafe extern "system" fn(*mut c_void, *mut i64) -> HRESULT,
    pub GetRegistrationProperties:
        unsafe extern "system" fn(*mut c_void, *mut *mut APO_REG_PROPERTIES) -> HRESULT,
    pub Initialize: unsafe extern "system" fn(*mut c_void, u32, *const u8) -> HRESULT,
    pub IsInputFormatSupported: unsafe extern "system" fn(
        *mut c_void,
        *mut IAudioMediaType,
        *mut IAudioMediaType,
        *mut *mut IAudioMediaType,
    ) -> HRESULT,
    pub IsOutputFormatSupported: unsafe extern "system" fn(
        *mut c_void,
        *mut IAudioMediaType,
        *mut IAudioMediaType,
        *mut *mut IAudioMediaType,
    ) -> HRESULT,
    pub GetInputChannelCount: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
}

#[repr(C)]
pub struct IAudioProcessingObjectRTVtbl {
    pub QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub APOProcess: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *const *const APO_CONNECTION_PROPERTY,
        u32,
        *mut *mut APO_CONNECTION_PROPERTY,
    ),
    pub CalcInputFrames: unsafe extern "system" fn(*mut c_void, u32) -> u32,
    pub CalcOutputFrames: unsafe extern "system" fn(*mut c_void, u32) -> u32,
}

#[repr(C)]
pub struct IAudioProcessingObjectConfigurationVtbl {
    pub QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub LockForProcess: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *const *const APO_CONNECTION_DESCRIPTOR,
        u32,
        *const *const APO_CONNECTION_DESCRIPTOR,
    ) -> HRESULT,
    pub UnlockForProcess: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

#[repr(C)]
pub struct IAudioSystemEffects2Vtbl {
    pub QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub GetEffectsList:
        unsafe extern "system" fn(*mut c_void, *mut *mut GUID, *mut u32, usize) -> HRESULT,
}

// ============================================================================
// Global DLL Durumu
// ============================================================================

static DLL_REF_COUNT: AtomicI32 = AtomicI32::new(0);
static DLL_INSTANCE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

#[inline(always)]
fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

/// Gerçek zamanlı `APOProcess` sırasında sıfır bellek ayırma ile çalışan
/// ön-ayrılmış `rubato` yeniden örnekleme ve kanal eşitleme motoru.
const MAX_RT_FRAMES: usize = 4096;

pub struct PreallocatedResampler {
    /// Kaynak hız (Ozan mikseri sabit 48000 Hz stereo üretir veya paylaşımlı bellekten okunur)
    pub src_rate: u32,
    /// Hedef mikrofon hızı (örn. 48000, 44100, 16000 Bluetooth)
    pub dst_rate: u32,
    pub dst_channels: usize,
    pub resampler: FastFixedIn<f32>,
    /// Ön-ayrılmış stereo giriş kanalları `[L, R]` (`MAX_RT_FRAMES` kapasiteli)
    pub in_planar: Vec<Vec<f32>>,
    /// Ön-ayrılmış stereo çıkış kanalları `[L, R]` (`MAX_RT_FRAMES * 2` kapasiteli)
    pub out_planar: Vec<Vec<f32>>,
    /// Ön-ayrılmış paylaşımlı bellek okuma tamponu (`MAX_RT_FRAMES * 2` örnek)
    pub raw_interleaved: Vec<f32>,
}

impl PreallocatedResampler {
    /// `LockForProcess` sırasında (gerçek zamanlı döngüye girmeden önce) bir kez ayrılır.
    pub fn new(src_rate: u32, dst_rate: u32, dst_channels: usize, max_frames: usize) -> Self {
        let cap_frames = max_frames.max(MAX_RT_FRAMES);
        let ratio = (dst_rate.max(8_000) as f64) / (src_rate.max(8_000) as f64);
        let resampler = FastFixedIn::new(
            ratio,
            2.0,
            PolynomialDegree::Cubic,
            cap_frames,
            2,
        )
        .unwrap_or_else(|_| {
            FastFixedIn::new(1.0, 2.0, PolynomialDegree::Linear, cap_frames, 2).unwrap()
        });
        Self {
            src_rate,
            dst_rate,
            dst_channels: dst_channels.max(1),
            resampler,
            in_planar: vec![vec![0.0f32; cap_frames]; 2],
            out_planar: vec![vec![0.0f32; cap_frames * 2 + 64]; 2],
            raw_interleaved: vec![0.0f32; cap_frames * 2],
        }
    }

    /// Gerçek zamanlı `APOProcess` içinde sıfır bellek ayırma (`process_into_buffer`) ile
    /// örnekleri yeniden örnekler ve hedef kanal sayısına (Mono / Stereo / Çok Kanallı) karıştırır.
    #[inline]
    pub unsafe fn process_and_mix_rt(
        &mut self,
        shm: &OzanSharedBlock,
        out_f32: *mut f32,
        frames: usize,
        channels: usize,
        gain: f32,
    ) {
        let frames = frames.min(self.in_planar[0].len());
        if frames == 0 {
            return;
        }

        // Eğer üretici (`ozan.exe`) zaten APO'nun hedef örnekleme hızı ve kanal sayısında
        // örnek yazıyorsa doğrudan halka tampondan sıfır kopya ile karıştır!
        let shm_rate = shm.apo_sample_rate.load(Ordering::Relaxed);
        let shm_ch = shm.apo_channels.load(Ordering::Relaxed) as usize;
        if shm_rate == self.dst_rate && shm_ch == channels {
            shm.mix_into_raw(out_f32, frames * channels, gain);
            return;
        }

        // Farklı örnekleme hızı veya kanal düzeni durumunda `rubato::Resampler::process_into_buffer`
        // kullanarak ön-ayrılmış tamponlarda sıfır heap ayırma ile dönüştür.
        let ratio = (self.dst_rate as f64) / (self.src_rate.max(1) as f64);
        let needed_in_frames = ((frames as f64 / ratio).ceil() as usize).clamp(1, frames);
        let read_samples = shm.pop_samples(&mut self.raw_interleaved[..needed_in_frames * 2]);
        let got_in_frames = read_samples / 2;
        if got_in_frames == 0 {
            return;
        }

        for i in 0..got_in_frames {
            self.in_planar[0][i] = self.raw_interleaved[i * 2];
            self.in_planar[1][i] = self.raw_interleaved[i * 2 + 1];
        }

        let idx = Indexing {
            input_offset: 0,
            output_offset: 0,
            active_channels_mask: None,
            partial_len: Some(got_in_frames),
        };

        if let Ok((_, out_frames)) =
            self.resampler
                .process_into_buffer(&self.in_planar, &mut self.out_planar, Some(&idx))
        {
            let mix_frames = out_frames.min(frames);
            if channels == 1 {
                // Bluetooth 16 kHz Mono vb. için Stereo -> Mono (`0.5 * (L + R)`) eşitleme
                for f in 0..mix_frames {
                    let mono = 0.5 * (self.out_planar[0][f] + self.out_planar[1][f]) * gain;
                    let p = out_f32.add(f);
                    *p = (*p + mono).clamp(-1.0, 1.0);
                }
            } else {
                // Stereo veya çok kanallı mikrofon (ilk 2 kanala L/R enjekte et)
                for f in 0..mix_frames {
                    let l = self.out_planar[0][f] * gain;
                    let r = self.out_planar[1][f] * gain;
                    let base = out_f32.add(f * channels);
                    *base = (*base + l).clamp(-1.0, 1.0);
                    *base.add(1) = (*base.add(1) + r).clamp(-1.0, 1.0);
                }
            }
        }
    }
}

/// Zincirlenen (Chained) Orijinal Üretici APO Bağlantısı.
/// Aktif mikrofonun mevcut üretici ses efektlerini (gürültü bastırma vb.) korur.
pub struct ChainedManufacturerApo {
    pub apo: *mut c_void,
    pub apo_rt: *mut c_void,
    pub apo_cfg: *mut c_void,
}

unsafe impl Send for ChainedManufacturerApo {}
unsafe impl Sync for ChainedManufacturerApo {}

impl ChainedManufacturerApo {
    /// Orijinal üretici APO'sunu CLSID üzerinden yükler (`LockForProcess` / `Initialize` aşamasında).
    pub unsafe fn try_load(clsid_utf16: &[u16]) -> Option<Self> {
        let len = clsid_utf16.iter().position(|&c| c == 0).unwrap_or(clsid_utf16.len());
        if len < 38 {
            return None;
        }
        let mut clsid: GUID = zeroed();
        if CLSIDFromString(clsid_utf16.as_ptr(), &mut clsid) != S_OK {
            return None;
        }
        // Sonsuz döngüyü önle: kendi CLSID'mizi asla kendimize zincirleme!
        if guid_eq(&clsid, &OZAN_APO_CLSID) {
            return None;
        }

        let mut apo: *mut c_void = null_mut();
        let hr = CoCreateInstance(
            &clsid,
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IAUDIOPROCESSINGOBJECT,
            &mut apo,
        );
        if hr != S_OK || apo.is_null() {
            return None;
        }

        let vtbl = *(apo as *const *const IAudioProcessingObjectVtbl);
        let mut apo_rt: *mut c_void = null_mut();
        let _ = ((*vtbl).QueryInterface)(apo, &IID_IAUDIOPROCESSINGOBJECT_RT, &mut apo_rt);

        let mut apo_cfg: *mut c_void = null_mut();
        let _ = ((*vtbl).QueryInterface)(apo, &IID_IAUDIOPROCESSINGOBJECT_CONFIG, &mut apo_cfg);

        if apo_rt.is_null() {
            ((*vtbl).Release)(apo);
            if !apo_cfg.is_null() {
                let cfg_vtbl = *(apo_cfg as *const *const IUnknownVtbl);
                ((*cfg_vtbl).Release)(apo_cfg);
            }
            return None;
        }

        Some(Self {
            apo,
            apo_rt,
            apo_cfg,
        })
    }

    #[inline(always)]
    pub unsafe fn process_rt(
        &self,
        num_in: u32,
        in_conns: *const *const APO_CONNECTION_PROPERTY,
        num_out: u32,
        out_conns: *mut *mut APO_CONNECTION_PROPERTY,
    ) {
        if !self.apo_rt.is_null() {
            let vtbl = *(self.apo_rt as *const *const IAudioProcessingObjectRTVtbl);
            ((*vtbl).APOProcess)(self.apo_rt, num_in, in_conns, num_out, out_conns);
        }
    }
}

impl Drop for ChainedManufacturerApo {
    fn drop(&mut self) {
        unsafe {
            if !self.apo_rt.is_null() {
                let vtbl = *(self.apo_rt as *const *const IUnknownVtbl);
                ((*vtbl).Release)(self.apo_rt);
            }
            if !self.apo_cfg.is_null() {
                let vtbl = *(self.apo_cfg as *const *const IUnknownVtbl);
                ((*vtbl).Release)(self.apo_cfg);
            }
            if !self.apo.is_null() {
                let vtbl = *(self.apo as *const *const IUnknownVtbl);
                ((*vtbl).Release)(self.apo);
            }
        }
    }
}

// ============================================================================
// Ana `OzanApo` COM Nesnesi (Çoklu VTable Düzeni)
// ============================================================================

#[repr(C)]
pub struct OzanApo {
    pub vtbl_apo: *const IAudioProcessingObjectVtbl,
    pub vtbl_rt: *const IAudioProcessingObjectRTVtbl,
    pub vtbl_cfg: *const IAudioProcessingObjectConfigurationVtbl,
    pub vtbl_fx: *const IUnknownVtbl,
    pub vtbl_fx2: *const IAudioSystemEffects2Vtbl,
    pub ref_count: AtomicU32,
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
    pub format_tag: AtomicU32,
    pub is_locked: AtomicU32,
    pub shm: Option<SharedMemoryMapping>,
    pub shm_ptr: *const OzanSharedBlock,
    pub resampler: Option<PreallocatedResampler>,
    pub chained: Option<ChainedManufacturerApo>,
}

impl OzanApo {
    pub fn new_boxed() -> *mut Self {
        DLL_REF_COUNT.fetch_add(1, Ordering::Relaxed);
        let shm = SharedMemoryMapping::create_or_open().ok();
        let shm_ptr = shm.as_ref().map(|m| m.block() as *const OzanSharedBlock).unwrap_or(null());
        let resampler = Some(PreallocatedResampler::new(48_000, 48_000, 2, MAX_RT_FRAMES));

        let obj = Box::new(Self {
            vtbl_apo: &VTBL_APO,
            vtbl_rt: &VTBL_APO_RT,
            vtbl_cfg: &VTBL_APO_CFG,
            vtbl_fx: &VTBL_APO_FX,
            vtbl_fx2: &VTBL_APO_FX2,
            ref_count: AtomicU32::new(1),
            sample_rate: AtomicU32::new(48_000),
            channels: AtomicU32::new(2),
            format_tag: AtomicU32::new(APO_FORMAT_F32),
            is_locked: AtomicU32::new(0),
            shm,
            shm_ptr,
            resampler,
            chained: None,
        });
        Box::into_raw(obj)
    }

    #[inline(always)]
    unsafe fn from_apo(this: *mut c_void) -> *mut Self {
        (this as *mut u8).sub(offset_of!(Self, vtbl_apo)) as *mut Self
    }

    #[inline(always)]
    unsafe fn from_rt(this: *mut c_void) -> *mut Self {
        (this as *mut u8).sub(offset_of!(Self, vtbl_rt)) as *mut Self
    }

    #[inline(always)]
    unsafe fn from_cfg(this: *mut c_void) -> *mut Self {
        (this as *mut u8).sub(offset_of!(Self, vtbl_cfg)) as *mut Self
    }

    #[inline(always)]
    unsafe fn from_fx(this: *mut c_void) -> *mut Self {
        (this as *mut u8).sub(offset_of!(Self, vtbl_fx)) as *mut Self
    }

    #[inline(always)]
    unsafe fn from_fx2(this: *mut c_void) -> *mut Self {
        (this as *mut u8).sub(offset_of!(Self, vtbl_fx2)) as *mut Self
    }

    unsafe fn query_interface(&mut self, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
        if ppv.is_null() || riid.is_null() {
            return E_POINTER;
        }
        *ppv = null_mut();
        let iid = &*riid;

        if guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_IAUDIOPROCESSINGOBJECT) {
            *ppv = &mut self.vtbl_apo as *mut _ as *mut c_void;
        } else if guid_eq(iid, &IID_IAUDIOPROCESSINGOBJECT_RT) {
            *ppv = &mut self.vtbl_rt as *mut _ as *mut c_void;
        } else if guid_eq(iid, &IID_IAUDIOPROCESSINGOBJECT_CONFIG) {
            *ppv = &mut self.vtbl_cfg as *mut _ as *mut c_void;
        } else if guid_eq(iid, &IID_IAUDIOSYSTEMEFFECTS) {
            *ppv = &mut self.vtbl_fx as *mut _ as *mut c_void;
        } else if guid_eq(iid, &IID_IAUDIOSYSTEMEFFECTS2) {
            *ppv = &mut self.vtbl_fx2 as *mut _ as *mut c_void;
        } else {
            return E_NOINTERFACE;
        }

        self.add_ref();
        S_OK
    }

    #[inline]
    fn add_ref(&self) -> u32 {
        self.ref_count.fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe fn release(this: *mut Self) -> u32 {
        let prev = (*this).ref_count.fetch_sub(1, Ordering::Release);
        if prev == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            if !(*this).shm_ptr.is_null() {
                (*(*this).shm_ptr)
                    .flags
                    .fetch_and(!FLAG_APO_ACTIVE, Ordering::Release);
            }
            drop(Box::from_raw(this));
            DLL_REF_COUNT.fetch_sub(1, Ordering::Relaxed);
            0
        } else {
            prev - 1
        }
    }
}

// ============================================================================
// IAudioProcessingObject Metotları
// ============================================================================

unsafe extern "system" fn apo_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    (*OzanApo::from_apo(this)).query_interface(riid, ppv)
}

unsafe extern "system" fn apo_add_ref(this: *mut c_void) -> u32 {
    (*OzanApo::from_apo(this)).add_ref()
}

unsafe extern "system" fn apo_release(this: *mut c_void) -> u32 {
    OzanApo::release(OzanApo::from_apo(this))
}

unsafe extern "system" fn apo_reset(this: *mut c_void) -> HRESULT {
    let me = &mut *OzanApo::from_apo(this);
    if let Some(rs) = me.resampler.as_mut() {
        rs.resampler.reset();
    }
    if let Some(ch) = me.chained.as_ref() {
        let vtbl = *(ch.apo as *const *const IAudioProcessingObjectVtbl);
        let _ = ((*vtbl).Reset)(ch.apo);
    }
    S_OK
}

unsafe extern "system" fn apo_get_latency(this: *mut c_void, p_time: *mut i64) -> HRESULT {
    if p_time.is_null() {
        return E_POINTER;
    }
    let me = &*OzanApo::from_apo(this);
    if let Some(ch) = me.chained.as_ref() {
        let vtbl = *(ch.apo as *const *const IAudioProcessingObjectVtbl);
        return ((*vtbl).GetLatency)(ch.apo, p_time);
    }
    *p_time = 0;
    S_OK
}

unsafe extern "system" fn apo_get_registration_properties(
    _this: *mut c_void,
    pp_reg_props: *mut *mut APO_REG_PROPERTIES,
) -> HRESULT {
    if pp_reg_props.is_null() {
        return E_POINTER;
    }
    let ptr = CoTaskMemAlloc(size_of::<APO_REG_PROPERTIES>()) as *mut APO_REG_PROPERTIES;
    if ptr.is_null() {
        return E_OUTOFMEMORY;
    }
    ptr.write_bytes(0, 1);
    let props = &mut *ptr;
    props.clsid = OZAN_APO_CLSID;
    props.Flags = APO_FLAG_DEFAULT;
    for (i, ch) in "Ozan Direct Microphone Injector APO".encode_utf16().take(255).enumerate() {
        props.szFriendlyName[i] = ch;
    }
    for (i, ch) in "Ozan Audio Engine (APO Chaining)".encode_utf16().take(255).enumerate() {
        props.szCopyrightInfo[i] = ch;
    }
    props.u32MajorVersion = 1;
    props.u32MinorVersion = 0;
    props.u32MinInputConnections = 1;
    props.u32MaxInputConnections = 1;
    props.u32MinOutputConnections = 1;
    props.u32MaxOutputConnections = 1;
    props.u32MaxInstances = 0xFFFF_FFFF;
    props.u32NumAPOInterfaces = 1;
    props.iidAPOInterfaceList[0] = IID_IAUDIOSYSTEMEFFECTS;
    *pp_reg_props = ptr;
    S_OK
}

unsafe extern "system" fn apo_initialize(
    this: *mut c_void,
    cb_data_size: u32,
    pby_data: *const u8,
) -> HRESULT {
    let me = &mut *OzanApo::from_apo(this);
    if me.shm.is_none() {
        me.shm = SharedMemoryMapping::create_or_open().ok();
        me.shm_ptr = me
            .shm
            .as_ref()
            .map(|m| m.block() as *const OzanSharedBlock)
            .unwrap_or(null());
    }
    if !me.shm_ptr.is_null() {
        let shm = &*me.shm_ptr;
        let mut clsid_buf = [0u16; 40];
        if shm.read_chained_clsid_buf(&mut clsid_buf) > 0 && me.chained.is_none() {
            me.chained = ChainedManufacturerApo::try_load(&clsid_buf);
        }
    }
    if let Some(ch) = me.chained.as_ref() {
        let vtbl = *(ch.apo as *const *const IAudioProcessingObjectVtbl);
        let _ = ((*vtbl).Initialize)(ch.apo, cb_data_size, pby_data);
    }
    S_OK
}

unsafe extern "system" fn apo_is_input_format_supported(
    this: *mut c_void,
    p_opposite_format: *mut IAudioMediaType,
    p_requested_input_format: *mut IAudioMediaType,
    pp_supported_input_format: *mut *mut IAudioMediaType,
) -> HRESULT {
    if p_requested_input_format.is_null() || pp_supported_input_format.is_null() {
        return E_POINTER;
    }
    let me = &*OzanApo::from_apo(this);
    if let Some(ch) = me.chained.as_ref() {
        let vtbl = *(ch.apo as *const *const IAudioProcessingObjectVtbl);
        return ((*vtbl).IsInputFormatSupported)(
            ch.apo,
            p_opposite_format,
            p_requested_input_format,
            pp_supported_input_format,
        );
    }
    let req_vtbl = *((*p_requested_input_format).lpVtbl);
    let _ = (req_vtbl.AddRef)(p_requested_input_format as *mut c_void);
    *pp_supported_input_format = p_requested_input_format;
    S_OK
}

unsafe extern "system" fn apo_is_output_format_supported(
    this: *mut c_void,
    p_opposite_format: *mut IAudioMediaType,
    p_requested_output_format: *mut IAudioMediaType,
    pp_supported_output_format: *mut *mut IAudioMediaType,
) -> HRESULT {
    if p_requested_output_format.is_null() || pp_supported_output_format.is_null() {
        return E_POINTER;
    }
    let me = &*OzanApo::from_apo(this);
    if let Some(ch) = me.chained.as_ref() {
        let vtbl = *(ch.apo as *const *const IAudioProcessingObjectVtbl);
        return ((*vtbl).IsOutputFormatSupported)(
            ch.apo,
            p_opposite_format,
            p_requested_output_format,
            pp_supported_output_format,
        );
    }
    let req_vtbl = *((*p_requested_output_format).lpVtbl);
    let _ = (req_vtbl.AddRef)(p_requested_output_format as *mut c_void);
    *pp_supported_output_format = p_requested_output_format;
    S_OK
}

unsafe extern "system" fn apo_get_input_channel_count(
    this: *mut c_void,
    pu32_channel_count: *mut u32,
) -> HRESULT {
    if pu32_channel_count.is_null() {
        return E_POINTER;
    }
    let me = &*OzanApo::from_apo(this);
    *pu32_channel_count = me.channels.load(Ordering::Relaxed);
    S_OK
}

// ============================================================================
// IAudioProcessingObjectConfiguration Metotları (`LockForProcess` / `UnlockForProcess`)
// ============================================================================

unsafe extern "system" fn cfg_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    (*OzanApo::from_cfg(this)).query_interface(riid, ppv)
}

unsafe extern "system" fn cfg_add_ref(this: *mut c_void) -> u32 {
    (*OzanApo::from_cfg(this)).add_ref()
}

unsafe extern "system" fn cfg_release(this: *mut c_void) -> u32 {
    OzanApo::release(OzanApo::from_cfg(this))
}

unsafe extern "system" fn cfg_lock_for_process(
    this: *mut c_void,
    num_in: u32,
    in_descs: *const *const APO_CONNECTION_DESCRIPTOR,
    num_out: u32,
    out_descs: *const *const APO_CONNECTION_DESCRIPTOR,
) -> HRESULT {
    if num_in == 0 || in_descs.is_null() || num_out == 0 || out_descs.is_null() {
        return E_INVALIDARG;
    }
    let me = &mut *OzanApo::from_cfg(this);
    let in_desc = *in_descs;
    let mut max_frames = MAX_RT_FRAMES;
    if !in_desc.is_null() {
        max_frames = ((*in_desc).u32MaxFrameCount as usize).max(MAX_RT_FRAMES);
        let fmt_ptr = (*in_desc).pFormat;
        if !fmt_ptr.is_null() && !(*fmt_ptr).lpVtbl.is_null() {
            let mut ufmt: UNCOMPRESSED_AUDIOMEDIATYPE = zeroed();
            let vtbl = *(*fmt_ptr).lpVtbl;
            if (vtbl.GetUncompressedAudioFormat)(fmt_ptr as *mut c_void, &mut ufmt) == S_OK {
                let rate = (ufmt.fFramesPerSecond.round() as u32).clamp(8_000, 384_000);
                let ch = ufmt.dwSamplesPerFrame.clamp(1, 16);
                let ftag = if ufmt.dwBytesPerSampleContainer == 2
                    || guid_eq(&ufmt.guidFormatType, &KSDATAFORMAT_SUBTYPE_PCM)
                {
                    APO_FORMAT_I16
                } else {
                    APO_FORMAT_F32
                };
                me.sample_rate.store(rate, Ordering::Relaxed);
                me.channels.store(ch, Ordering::Relaxed);
                me.format_tag.store(ftag, Ordering::Relaxed);
            }
        }
    }

    let dst_rate = me.sample_rate.load(Ordering::Relaxed);
    let dst_ch = me.channels.load(Ordering::Relaxed) as usize;
    let dst_fmt = me.format_tag.load(Ordering::Relaxed);

    // `rubato` yeniden örnekleyiciyi `LockForProcess` sırasında ön-ayır (APOProcess'te sıfır allocation)
    me.resampler = Some(PreallocatedResampler::new(48_000, dst_rate, dst_ch, max_frames));

    if me.shm.is_none() {
        me.shm = SharedMemoryMapping::create_or_open().ok();
        me.shm_ptr = me
            .shm
            .as_ref()
            .map(|m| m.block() as *const OzanSharedBlock)
            .unwrap_or(null());
    }
    if !me.shm_ptr.is_null() {
        let shm = &*me.shm_ptr;
        shm.apo_sample_rate.store(dst_rate, Ordering::Release);
        shm.apo_channels.store(dst_ch as u32, Ordering::Release);
        shm.apo_format.store(dst_fmt, Ordering::Release);
        shm.flags.fetch_or(FLAG_APO_ACTIVE, Ordering::Release);

        let mut clsid_buf = [0u16; 40];
        if shm.read_chained_clsid_buf(&mut clsid_buf) > 0 && me.chained.is_none() {
            me.chained = ChainedManufacturerApo::try_load(&clsid_buf);
        }
    }

    if let Some(ch) = me.chained.as_ref() {
        if !ch.apo_cfg.is_null() {
            let vtbl = *(ch.apo_cfg as *const *const IAudioProcessingObjectConfigurationVtbl);
            let _ = ((*vtbl).LockForProcess)(ch.apo_cfg, num_in, in_descs, num_out, out_descs);
        }
    }

    me.is_locked.store(1, Ordering::Release);
    S_OK
}

unsafe extern "system" fn cfg_unlock_for_process(this: *mut c_void) -> HRESULT {
    let me = &mut *OzanApo::from_cfg(this);
    me.is_locked.store(0, Ordering::Release);
    if !me.shm_ptr.is_null() {
        (*me.shm_ptr)
            .flags
            .fetch_and(!FLAG_APO_ACTIVE, Ordering::Release);
    }
    if let Some(ch) = me.chained.as_ref() {
        if !ch.apo_cfg.is_null() {
            let vtbl = *(ch.apo_cfg as *const *const IAudioProcessingObjectConfigurationVtbl);
            let _ = ((*vtbl).UnlockForProcess)(ch.apo_cfg);
        }
    }
    S_OK
}

// ============================================================================
// IAudioProcessingObjectRT Metotları (GERÇEK ZAMANLI `APOProcess`)
// ============================================================================

unsafe extern "system" fn rt_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    (*OzanApo::from_rt(this)).query_interface(riid, ppv)
}

unsafe extern "system" fn rt_add_ref(this: *mut c_void) -> u32 {
    (*OzanApo::from_rt(this)).add_ref()
}

unsafe extern "system" fn rt_release(this: *mut c_void) -> u32 {
    OzanApo::release(OzanApo::from_rt(this))
}

/// Gerçek zamanlı ses işleme döngüsü (`audiodg.exe` gerçek zamanlı iş parçacığı).
///
/// KESİN KURALLAR:
/// - Hiçbir heap bellek ayırma (`Box`, `Vec`, `String`) yapılmaz.
/// - Hiçbir kilit (`Mutex`, `RwLock`) alınmaz.
/// - Hiçbir disk veya kayıt defteri okuması yapılmaz.
unsafe extern "system" fn rt_apo_process(
    this: *mut c_void,
    num_in: u32,
    in_conns: *const *const APO_CONNECTION_PROPERTY,
    num_out: u32,
    out_conns: *mut *mut APO_CONNECTION_PROPERTY,
) {
    if num_in == 0 || in_conns.is_null() || num_out == 0 || out_conns.is_null() {
        return;
    }
    let in_prop = *in_conns;
    let out_prop = *out_conns;
    if in_prop.is_null() || out_prop.is_null() {
        return;
    }

    let me = &mut *OzanApo::from_rt(this);

    // 1. Orijinal üretici APO zinciri (APO Chaining) varsa önce fiziksel mikrofonu ona işlet
    if let Some(chained) = me.chained.as_ref() {
        chained.process_rt(num_in, in_conns, num_out, out_conns);
    }

    let frames = (*in_prop).u32ValidFrameCount as usize;
    (*out_prop).u32ValidFrameCount = (*in_prop).u32ValidFrameCount;
    if frames == 0 || (*in_prop).pBuffer == 0 || (*out_prop).pBuffer == 0 {
        return;
    }

    let channels = (me.channels.load(Ordering::Relaxed) as usize).max(1);
    let total_samples = frames * channels;
    let fmt = me.format_tag.load(Ordering::Relaxed);

    // 2. Zincirlenen APO yoksa ve giriş/çıkış tamponları farklıysa mikrofon verisini kopyala
    if me.chained.is_none() {
        if (*in_prop).u32BufferFlags == BUFFER_SILENT {
            if fmt == APO_FORMAT_I16 {
                let out_i16 = (*out_prop).pBuffer as *mut i16;
                std::ptr::write_bytes(out_i16, 0, total_samples);
            } else {
                let out_f32 = (*out_prop).pBuffer as *mut f32;
                std::ptr::write_bytes(out_f32, 0, total_samples);
            }
            (*out_prop).u32BufferFlags = BUFFER_SILENT;
        } else if (*in_prop).pBuffer != (*out_prop).pBuffer {
            if fmt == APO_FORMAT_I16 {
                std::ptr::copy_nonoverlapping(
                    (*in_prop).pBuffer as *const i16,
                    (*out_prop).pBuffer as *mut i16,
                    total_samples,
                );
            } else {
                std::ptr::copy_nonoverlapping(
                    (*in_prop).pBuffer as *const f32,
                    (*out_prop).pBuffer as *mut f32,
                    total_samples,
                );
            }
            (*out_prop).u32BufferFlags = (*in_prop).u32BufferFlags;
        }
    }

    // 3. Paylaşımlı bellek (`OzanAudioSharedMem_v1`) kontrolü ve kilitsiz ses enjeksiyonu
    if me.shm_ptr.is_null() {
        return;
    }
    let shm = &*me.shm_ptr;
    shm.apo_heartbeat.fetch_add(1, Ordering::Relaxed);

    let flags = shm.flags.load(Ordering::Acquire);
    if (flags & FLAG_LOCAL_PREVIEW_ONLY) != 0 {
        return;
    }

    // Fiziksel mikrofonu geçici susturma (`mute_mic_on_play`) aktifse çıkış tamponunu sıfırla
    if (flags & FLAG_MUTE_PHYSICAL_MIC) != 0 {
        if fmt == APO_FORMAT_I16 {
            std::ptr::write_bytes((*out_prop).pBuffer as *mut i16, 0, total_samples);
        } else {
            std::ptr::write_bytes((*out_prop).pBuffer as *mut f32, 0, total_samples);
        }
    }

    if shm.available_read() == 0 {
        return;
    }

    let gain = f32::from_bits(shm.inject_gain_bits.load(Ordering::Relaxed)).clamp(0.0, 4.0);

    if fmt == APO_FORMAT_F32 {
        let out_f32 = (*out_prop).pBuffer as *mut f32;
        if (*out_prop).u32BufferFlags == BUFFER_SILENT && (flags & FLAG_MUTE_PHYSICAL_MIC) == 0 {
            std::ptr::write_bytes(out_f32, 0, total_samples);
        }
        if let Some(rs) = me.resampler.as_mut() {
            rs.process_and_mix_rt(shm, out_f32, frames, channels, gain);
        } else {
            shm.mix_into_raw(out_f32, total_samples, gain);
        }
        (*out_prop).u32BufferFlags = BUFFER_VALID;
    } else if fmt == APO_FORMAT_I16 {
        let out_i16 = (*out_prop).pBuffer as *mut i16;
        if (*out_prop).u32BufferFlags == BUFFER_SILENT && (flags & FLAG_MUTE_PHYSICAL_MIC) == 0 {
            std::ptr::write_bytes(out_i16, 0, total_samples);
        }
        if let Some(rs) = me.resampler.as_mut() {
            let cap = rs.raw_interleaved.len().min(total_samples);
            let n = shm.pop_samples(&mut rs.raw_interleaved[..cap]);
            for i in 0..n {
                let p = out_i16.add(i);
                let cur = (*p as f32) / 32768.0;
                let mixed = (cur + rs.raw_interleaved[i] * gain).clamp(-1.0, 1.0);
                *p = (mixed * 32767.0) as i16;
            }
            if n > 0 {
                (*out_prop).u32BufferFlags = BUFFER_VALID;
            }
        }
    }
}

unsafe extern "system" fn rt_calc_input_frames(_this: *mut c_void, out_frames: u32) -> u32 {
    out_frames
}

unsafe extern "system" fn rt_calc_output_frames(_this: *mut c_void, in_frames: u32) -> u32 {
    in_frames
}

// ============================================================================
// IAudioSystemEffects & IAudioSystemEffects2 Metotları
// ============================================================================

unsafe extern "system" fn fx_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    (*OzanApo::from_fx(this)).query_interface(riid, ppv)
}

unsafe extern "system" fn fx_add_ref(this: *mut c_void) -> u32 {
    (*OzanApo::from_fx(this)).add_ref()
}

unsafe extern "system" fn fx_release(this: *mut c_void) -> u32 {
    OzanApo::release(OzanApo::from_fx(this))
}

unsafe extern "system" fn fx2_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    (*OzanApo::from_fx2(this)).query_interface(riid, ppv)
}

unsafe extern "system" fn fx2_add_ref(this: *mut c_void) -> u32 {
    (*OzanApo::from_fx2(this)).add_ref()
}

unsafe extern "system" fn fx2_release(this: *mut c_void) -> u32 {
    OzanApo::release(OzanApo::from_fx2(this))
}

unsafe extern "system" fn fx2_get_effects_list(
    _this: *mut c_void,
    pp_effects_ids: *mut *mut GUID,
    pc_effects: *mut u32,
    _event: usize,
) -> HRESULT {
    if !pp_effects_ids.is_null() {
        *pp_effects_ids = null_mut();
    }
    if !pc_effects.is_null() {
        *pc_effects = 0;
    }
    S_OK
}

// ============================================================================
// Statik VTable Tabloları
// ============================================================================

static VTBL_APO: IAudioProcessingObjectVtbl = IAudioProcessingObjectVtbl {
    QueryInterface: apo_qi,
    AddRef: apo_add_ref,
    Release: apo_release,
    Reset: apo_reset,
    GetLatency: apo_get_latency,
    GetRegistrationProperties: apo_get_registration_properties,
    Initialize: apo_initialize,
    IsInputFormatSupported: apo_is_input_format_supported,
    IsOutputFormatSupported: apo_is_output_format_supported,
    GetInputChannelCount: apo_get_input_channel_count,
};

static VTBL_APO_RT: IAudioProcessingObjectRTVtbl = IAudioProcessingObjectRTVtbl {
    QueryInterface: rt_qi,
    AddRef: rt_add_ref,
    Release: rt_release,
    APOProcess: rt_apo_process,
    CalcInputFrames: rt_calc_input_frames,
    CalcOutputFrames: rt_calc_output_frames,
};

static VTBL_APO_CFG: IAudioProcessingObjectConfigurationVtbl =
    IAudioProcessingObjectConfigurationVtbl {
        QueryInterface: cfg_qi,
        AddRef: cfg_add_ref,
        Release: cfg_release,
        LockForProcess: cfg_lock_for_process,
        UnlockForProcess: cfg_unlock_for_process,
    };

static VTBL_APO_FX: IUnknownVtbl = IUnknownVtbl {
    QueryInterface: fx_qi,
    AddRef: fx_add_ref,
    Release: fx_release,
};

static VTBL_APO_FX2: IAudioSystemEffects2Vtbl = IAudioSystemEffects2Vtbl {
    QueryInterface: fx2_qi,
    AddRef: fx2_add_ref,
    Release: fx2_release,
    GetEffectsList: fx2_get_effects_list,
};

// ============================================================================
// IClassFactory Uygulaması
// ============================================================================

#[repr(C)]
pub struct OzanClassFactory {
    pub lpVtbl: *const IClassFactoryVtbl,
    pub ref_count: AtomicU32,
}

unsafe extern "system" fn cf_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = &*riid;
    if guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_ICLASSFACTORY) {
        *ppv = this;
        cf_add_ref(this);
        S_OK
    } else {
        *ppv = null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn cf_add_ref(this: *mut c_void) -> u32 {
    let cf = &*(this as *mut OzanClassFactory);
    cf.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn cf_release(this: *mut c_void) -> u32 {
    let cf = this as *mut OzanClassFactory;
    let prev = (*cf).ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(Box::from_raw(cf));
        DLL_REF_COUNT.fetch_sub(1, Ordering::Relaxed);
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn cf_create_instance(
    _this: *mut c_void,
    p_unk_outer: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = null_mut();
    if !p_unk_outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    let apo = OzanApo::new_boxed();
    let hr = (*apo).query_interface(riid, ppv);
    OzanApo::release(apo);
    hr
}

unsafe extern "system" fn cf_lock_server(_this: *mut c_void, f_lock: BOOL) -> HRESULT {
    if f_lock != 0 {
        DLL_REF_COUNT.fetch_add(1, Ordering::Relaxed);
    } else {
        DLL_REF_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
    S_OK
}

static VTBL_CLASS_FACTORY: IClassFactoryVtbl = IClassFactoryVtbl {
    QueryInterface: cf_qi,
    AddRef: cf_add_ref,
    Release: cf_release,
    CreateInstance: cf_create_instance,
    LockServer: cf_lock_server,
};

// ============================================================================
// Standart COM DLL Dışa Aktarımları (`ozan-apo.def`)
// ============================================================================

#[no_mangle]
pub unsafe extern "system" fn DllMain(
    hinst_dll: HINSTANCE,
    fdw_reason: u32,
    _lpv_reserved: *mut c_void,
) -> BOOL {
    if fdw_reason == 1 {
        // DLL_PROCESS_ATTACH
        DLL_INSTANCE.store(hinst_dll, Ordering::Relaxed);
    }
    TRUE
}

#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    *ppv = null_mut();
    if !guid_eq(&*rclsid, &OZAN_APO_CLSID) {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    DLL_REF_COUNT.fetch_add(1, Ordering::Relaxed);
    let factory = Box::into_raw(Box::new(OzanClassFactory {
        lpVtbl: &VTBL_CLASS_FACTORY,
        ref_count: AtomicU32::new(1),
    }));
    let hr = cf_qi(factory as *mut c_void, riid, ppv);
    cf_release(factory as *mut c_void);
    hr
}

#[no_mangle]
pub unsafe extern "system" fn DllCanUnloadNow() -> HRESULT {
    if DLL_REF_COUNT.load(Ordering::Relaxed) <= 0 {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe fn register_clsid_in_root(root: HKEY, dll_path: &[u16]) -> bool {
    let subkey: Vec<u16> = format!("SOFTWARE\\Classes\\CLSID\\{OZAN_APO_CLSID_STR}\\InprocServer32\0")
        .encode_utf16()
        .collect();
    let mut hkey: HKEY = null_mut();
    if RegCreateKeyExW(
        root,
        subkey.as_ptr(),
        0,
        null(),
        REG_OPTION_NON_VOLATILE,
        KEY_WRITE,
        null(),
        &mut hkey,
        null_mut(),
    ) != 0
    {
        return false;
    }

    let _ = RegSetValueExW(
        hkey,
        null(),
        0,
        REG_SZ,
        dll_path.as_ptr() as *const u8,
        (dll_path.len() * 2) as u32,
    );
    let tm_name: Vec<u16> = "ThreadingModel\0".encode_utf16().collect();
    let tm_val: Vec<u16> = "Both\0".encode_utf16().collect();
    let _ = RegSetValueExW(
        hkey,
        tm_name.as_ptr(),
        0,
        REG_SZ,
        tm_val.as_ptr() as *const u8,
        (tm_val.len() * 2) as u32,
    );
    RegCloseKey(hkey);
    true
}

#[no_mangle]
pub unsafe extern "system" fn DllRegisterServer() -> HRESULT {
    let mut path_buf = [0u16; 1024];
    let hinst = DLL_INSTANCE.load(Ordering::Relaxed);
    let len = GetModuleFileNameW(hinst, path_buf.as_mut_ptr(), path_buf.len() as u32) as usize;
    if len == 0 {
        return E_INVALIDARG;
    }
    let slice = &path_buf[..=len];
    if register_clsid_in_root(HKEY_LOCAL_MACHINE, slice)
        || register_clsid_in_root(HKEY_CURRENT_USER, slice)
    {
        S_OK
    } else {
        E_INVALIDARG
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllUnregisterServer() -> HRESULT {
    let subkey: Vec<u16> = format!("SOFTWARE\\Classes\\CLSID\\{OZAN_APO_CLSID_STR}\0")
        .encode_utf16()
        .collect();
    let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, subkey.as_ptr());
    let _ = RegDeleteTreeW(HKEY_CURRENT_USER, subkey.as_ptr());
    S_OK
}
