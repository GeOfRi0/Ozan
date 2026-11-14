//! `IMMNotificationClient` Windows Core Audio bildirim istemcisi.
//!
//! USB / Bluetooth mikrofon takma-çıkarma ve varsayılan mikrofon değişimlerini
//! gerçek zamanlı yakalayarak `AudioEngine` ve APO yöneticisini otomatik olarak
//! yeni cihaza bağlar.

#![allow(non_snake_case, non_camel_case_types)]

use std::ffi::c_void;
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;

use windows_sys::core::{GUID, HRESULT, PCWSTR};
use windows_sys::Win32::Foundation::{E_NOINTERFACE, E_POINTER, PROPERTYKEY, S_OK};
use windows_sys::Win32::Media::Audio::{
    eCapture, eConsole, MMDeviceEnumerator, DEVICE_STATE_ACTIVE, EDataFlow, ERole, WAVEFORMATEX,
};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

type IMMDevice = c_void;
type IMMDeviceEnumerator = c_void;
type IPropertyStore = c_void;

#[link(name = "ole32")]
extern "system" {
    fn PropVariantClear(pvar: *mut PROPVARIANT_RAW) -> HRESULT;
}

/// `IID_IUnknown` `{00000000-0000-0000-C000-000000000046}`
const IID_IUNKNOWN: GUID = GUID {
    data1: 0x0000_0000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// `IID_IMMDeviceEnumerator` `{A95664D2-9614-4F35-A746-DE8DB63617E6}`
const IID_IMMDEVICE_ENUMERATOR: GUID = GUID {
    data1: 0xA956_64D2,
    data2: 0x9614,
    data3: 0x4F35,
    data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
};

/// `IID_IMMNotificationClient` `{7991EEC9-7E89-4D85-8390-6C703CEC60C0}`
const IID_IMMNOTIFICATION_CLIENT: GUID = GUID {
    data1: 0x7991_EEC9,
    data2: 0x7E89,
    data3: 0x4D85,
    data4: [0x83, 0x90, 0x6C, 0x70, 0x3C, 0xEC, 0x60, 0xC0],
};

/// `PKEY_Device_FriendlyName` `{a45c254e-df1c-4efd-8020-67d146a850e0}, 14`
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID {
        data1: 0xA45C_254E,
        data2: 0xDF1C,
        data3: 0x4EFD,
        data4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
    },
    pid: 14,
};

/// `PKEY_AudioEngine_DeviceFormat` `{f19f064d-082c-4e27-bc73-6882a1bb8e4c}, 0`
const PKEY_AUDIOENGINE_DEVICE_FORMAT: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID {
        data1: 0xF19F_064D,
        data2: 0x082C,
        data3: 0x4E27,
        data4: [0xBC, 0x73, 0x68, 0x82, 0xA1, 0xBB, 0x8E, 0x4C],
    },
    pid: 0,
};

/// Aktif fiziksel mikrofon uç noktası bilgileri.
#[derive(Debug, Clone)]
pub struct CaptureEndpointInfo {
    /// Tam uç nokta kimliği, örn. `"{0.0.1.00000000}.{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}"`
    pub full_id: String,
    /// Kayıt defteri `Capture\{GUID}` altında kullanılan saf GUID kısmı
    pub endpoint_guid: String,
    /// Kullanıcı dostu cihaz adı
    pub friendly_name: String,
    /// Cihazın yerel örnekleme hızı (örn. 48000, 44100, 16000)
    pub sample_rate: u32,
    /// Cihazın yerel kanal sayısı (1 = Mono, 2 = Stereo vb.)
    pub channels: u32,
}

#[derive(Debug, Clone)]
pub enum EndpointEvent {
    DefaultCaptureChanged(Option<CaptureEndpointInfo>),
    DeviceHotplug,
}

// ============================================================================
// Minimal COM VTable Bağlamaları (`IMMDeviceEnumerator`, `IMMDevice`, `IPropertyStore`)
// ============================================================================

#[repr(C)]
struct IMMDeviceEnumeratorVtbl {
    QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    Release: unsafe extern "system" fn(*mut c_void) -> u32,
    EnumAudioEndpoints:
        unsafe extern "system" fn(*mut c_void, EDataFlow, u32, *mut *mut c_void) -> HRESULT,
    GetDefaultAudioEndpoint:
        unsafe extern "system" fn(*mut c_void, EDataFlow, ERole, *mut *mut IMMDevice) -> HRESULT,
    GetDevice: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut *mut IMMDevice) -> HRESULT,
    RegisterEndpointNotificationCallback:
        unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    UnregisterEndpointNotificationCallback:
        unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
}

#[repr(C)]
struct IMMDeviceVtbl {
    QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    Release: unsafe extern "system" fn(*mut c_void) -> u32,
    Activate: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        u32,
        *const c_void,
        *mut *mut c_void,
    ) -> HRESULT,
    OpenPropertyStore:
        unsafe extern "system" fn(*mut c_void, u32, *mut *mut IPropertyStore) -> HRESULT,
    GetId: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
    GetState: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
}

#[repr(C)]
struct PROPVARIANT_RAW {
    vt: u16,
    wReserved1: u16,
    wReserved2: u16,
    wReserved3: u16,
    data1: usize,
    data2: usize,
}

#[repr(C)]
struct IPropertyStoreVtbl {
    QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    Release: unsafe extern "system" fn(*mut c_void) -> u32,
    GetCount: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    GetAt: unsafe extern "system" fn(*mut c_void, u32, *mut PROPERTYKEY) -> HRESULT,
    GetValue: unsafe extern "system" fn(
        *mut c_void,
        *const PROPERTYKEY,
        *mut PROPVARIANT_RAW,
    ) -> HRESULT,
    SetValue: unsafe extern "system" fn(
        *mut c_void,
        *const PROPERTYKEY,
        *const PROPVARIANT_RAW,
    ) -> HRESULT,
    Commit: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

#[repr(C)]
struct IMMNotificationClientVtbl {
    QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    Release: unsafe extern "system" fn(*mut c_void) -> u32,
    OnDeviceStateChanged: unsafe extern "system" fn(*mut c_void, PCWSTR, u32) -> HRESULT,
    OnDeviceAdded: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    OnDeviceRemoved: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    OnDefaultDeviceChanged:
        unsafe extern "system" fn(*mut c_void, EDataFlow, ERole, PCWSTR) -> HRESULT,
    OnPropertyValueChanged: unsafe extern "system" fn(*mut c_void, PCWSTR, PROPERTYKEY) -> HRESULT,
}

#[inline]
fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

unsafe fn pwstr_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
}

/// `"{0.0.1.00000000}.{xxxxxxxx-xxxx-...}"` içinden saf `{xxxxxxxx-xxxx-...}` GUID kısmını ayıklar.
pub fn extract_endpoint_guid(full_id: &str) -> String {
    if let Some(pos) = full_id.rfind(".{") {
        full_id[pos + 1..].to_string()
    } else {
        full_id.to_string()
    }
}

/// Varsayılan fiziksel mikrofon (Capture) uç noktasının kimlik, isim ve yerel ses biçimini sorgular.
pub fn query_default_capture_endpoint() -> Option<CaptureEndpointInfo> {
    unsafe {
        let _ = CoInitializeEx(null(), COINIT_MULTITHREADED as u32);
        let mut enum_ptr: *mut IMMDeviceEnumerator = null_mut();
        let hr = CoCreateInstance(
            &MMDeviceEnumerator,
            null_mut(),
            CLSCTX_ALL,
            &IID_IMMDEVICE_ENUMERATOR,
            &mut enum_ptr as *mut _ as *mut *mut c_void,
        );
        if hr != S_OK || enum_ptr.is_null() {
            return None;
        }
        let enum_vtbl = *(enum_ptr as *const *const IMMDeviceEnumeratorVtbl);
        let mut dev_ptr: *mut IMMDevice = null_mut();
        let hr = ((*enum_vtbl).GetDefaultAudioEndpoint)(
            enum_ptr as *mut c_void,
            eCapture,
            eConsole,
            &mut dev_ptr,
        );
        if hr != S_OK || dev_ptr.is_null() {
            ((*enum_vtbl).Release)(enum_ptr as *mut c_void);
            return None;
        }

        let info = read_device_info(dev_ptr);

        let dev_vtbl = *(dev_ptr as *const *const IMMDeviceVtbl);
        ((*dev_vtbl).Release)(dev_ptr as *mut c_void);
        ((*enum_vtbl).Release)(enum_ptr as *mut c_void);
        info
    }
}

unsafe fn read_device_info(dev_ptr: *mut IMMDevice) -> Option<CaptureEndpointInfo> {
    let dev_vtbl = *(dev_ptr as *const *const IMMDeviceVtbl);
    let mut id_pwstr: *mut u16 = null_mut();
    if ((*dev_vtbl).GetId)(dev_ptr as *mut c_void, &mut id_pwstr) != S_OK || id_pwstr.is_null() {
        return None;
    }
    let full_id = pwstr_to_string(id_pwstr);
    CoTaskMemFree(id_pwstr as *const c_void);
    let endpoint_guid = extract_endpoint_guid(&full_id);

    let mut friendly_name = String::from("Varsayılan Mikrofon");
    let mut sample_rate = 48_000u32;
    let mut channels = 2u32;

    let mut store_ptr: *mut IPropertyStore = null_mut();
    if ((*dev_vtbl).OpenPropertyStore)(dev_ptr as *mut c_void, STGM_READ, &mut store_ptr) == S_OK
        && !store_ptr.is_null()
    {
        let store_vtbl = *(store_ptr as *const *const IPropertyStoreVtbl);

        // 1. Cihaz Adı (`PKEY_Device_FriendlyName`, VT_LPWSTR = 31)
        let mut pv_name: PROPVARIANT_RAW = zeroed();
        if ((*store_vtbl).GetValue)(
            store_ptr as *mut c_void,
            &PKEY_DEVICE_FRIENDLY_NAME,
            &mut pv_name,
        ) == S_OK
        {
            if pv_name.vt == 31 && pv_name.data1 != 0 {
                let s = pwstr_to_string(pv_name.data1 as *const u16);
                if !s.is_empty() {
                    friendly_name = s;
                }
            }
            let _ = PropVariantClear(&mut pv_name as *mut _ as *mut _);
        }

        // 2. Ses Motoru Cihaz Formatı (`PKEY_AudioEngine_DeviceFormat`, VT_BLOB = 65)
        let mut pv_fmt: PROPVARIANT_RAW = zeroed();
        if ((*store_vtbl).GetValue)(
            store_ptr as *mut c_void,
            &PKEY_AUDIOENGINE_DEVICE_FORMAT,
            &mut pv_fmt,
        ) == S_OK
        {
            // VT_BLOB: data1 = cbSize, data2 = pBlobData
            if pv_fmt.vt == 65
                && pv_fmt.data1 >= std::mem::size_of::<WAVEFORMATEX>()
                && pv_fmt.data2 != 0
            {
                let wf = &*(pv_fmt.data2 as *const WAVEFORMATEX);
                if wf.nSamplesPerSec >= 8_000 && wf.nSamplesPerSec <= 384_000 {
                    sample_rate = wf.nSamplesPerSec;
                }
                if wf.nChannels >= 1 && wf.nChannels <= 16 {
                    channels = wf.nChannels as u32;
                }
            }
            let _ = PropVariantClear(&mut pv_fmt as *mut _ as *mut _);
        }

        ((*store_vtbl).Release)(store_ptr as *mut c_void);
    }

    Some(CaptureEndpointInfo {
        full_id,
        endpoint_guid,
        friendly_name,
        sample_rate,
        channels,
    })
}

// ============================================================================
// `IMMNotificationClient` COM Nesnesi
// ============================================================================

#[repr(C)]
struct NotificationClient {
    lpVtbl: *const IMMNotificationClientVtbl,
    ref_count: AtomicU32,
    tx: Sender<EndpointEvent>,
}

unsafe extern "system" fn nc_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = &*riid;
    if guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_IMMNOTIFICATION_CLIENT) {
        *ppv = this;
        nc_add_ref(this);
        S_OK
    } else {
        *ppv = null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn nc_add_ref(this: *mut c_void) -> u32 {
    let me = &*(this as *mut NotificationClient);
    me.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn nc_release(this: *mut c_void) -> u32 {
    let me = this as *mut NotificationClient;
    let prev = (*me).ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(Box::from_raw(me));
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn nc_on_device_state_changed(
    this: *mut c_void,
    _pwstr_device_id: PCWSTR,
    dw_new_state: u32,
) -> HRESULT {
    let me = &*(this as *mut NotificationClient);
    if dw_new_state == DEVICE_STATE_ACTIVE || dw_new_state != DEVICE_STATE_ACTIVE {
        let _ = me.tx.send(EndpointEvent::DeviceHotplug);
    }
    S_OK
}

unsafe extern "system" fn nc_on_device_added(this: *mut c_void, _pwstr_device_id: PCWSTR) -> HRESULT {
    let me = &*(this as *mut NotificationClient);
    let _ = me.tx.send(EndpointEvent::DeviceHotplug);
    S_OK
}

unsafe extern "system" fn nc_on_device_removed(
    this: *mut c_void,
    _pwstr_device_id: PCWSTR,
) -> HRESULT {
    let me = &*(this as *mut NotificationClient);
    let _ = me.tx.send(EndpointEvent::DeviceHotplug);
    S_OK
}

unsafe extern "system" fn nc_on_default_device_changed(
    this: *mut c_void,
    flow: EDataFlow,
    role: ERole,
    _pwstr_default_device_id: PCWSTR,
) -> HRESULT {
    if flow == eCapture && role == eConsole {
        let me = &*(this as *mut NotificationClient);
        let info = query_default_capture_endpoint();
        let _ = me.tx.send(EndpointEvent::DefaultCaptureChanged(info));
    }
    S_OK
}

unsafe extern "system" fn nc_on_property_value_changed(
    _this: *mut c_void,
    _pwstr_device_id: PCWSTR,
    _key: PROPERTYKEY,
) -> HRESULT {
    S_OK
}

static VTBL_NOTIFICATION_CLIENT: IMMNotificationClientVtbl = IMMNotificationClientVtbl {
    QueryInterface: nc_qi,
    AddRef: nc_add_ref,
    Release: nc_release,
    OnDeviceStateChanged: nc_on_device_state_changed,
    OnDeviceAdded: nc_on_device_added,
    OnDeviceRemoved: nc_on_device_removed,
    OnDefaultDeviceChanged: nc_on_default_device_changed,
    OnPropertyValueChanged: nc_on_property_value_changed,
};

/// `IMMNotificationClient` aboneliğini başlatır; arka planda cihaz takma/çıkarma
/// ve varsayılan mikrofon değişimlerini `tx` kanalına iletir.
pub fn spawn_endpoint_watcher(tx: Sender<EndpointEvent>) {
    let _ = std::thread::Builder::new()
        .name("ozan-mmnotify".into())
        .spawn(move || unsafe {
            let _ = CoInitializeEx(null(), COINIT_MULTITHREADED as u32);
            let mut enum_ptr: *mut IMMDeviceEnumerator = null_mut();
            let hr = CoCreateInstance(
                &MMDeviceEnumerator,
                null_mut(),
                CLSCTX_ALL,
                &IID_IMMDEVICE_ENUMERATOR,
                &mut enum_ptr as *mut _ as *mut *mut c_void,
            );
            if hr != S_OK || enum_ptr.is_null() {
                log::warn!("IMMDeviceEnumerator oluşturulamadı: 0x{hr:08X}");
                return;
            }

            let client = Box::into_raw(Box::new(NotificationClient {
                lpVtbl: &VTBL_NOTIFICATION_CLIENT,
                ref_count: AtomicU32::new(1),
                tx,
            }));

            let enum_vtbl = *(enum_ptr as *const *const IMMDeviceEnumeratorVtbl);
            let reg_hr = ((*enum_vtbl).RegisterEndpointNotificationCallback)(
                enum_ptr as *mut c_void,
                client as *mut c_void,
            );
            if reg_hr != S_OK {
                log::warn!("RegisterEndpointNotificationCallback başarısız: 0x{reg_hr:08X}");
                nc_release(client as *mut c_void);
                ((*enum_vtbl).Release)(enum_ptr as *mut c_void);
                return;
            }

            // COM bildirimlerinin yaşaması için iş parçacığını uykuda tut
            loop {
                std::thread::park();
            }
        });
}
