//! Windows User-Mode APO (Audio Processing Object) Yöneticisi,
//! Kayıt Defteri (`FxProperties`) APO Zincirleme (APO Chaining),
//! SHA-256 Bütünlük Denetimi, Kendini Onarma (Self-Healing) ve
//! Yedek Mod ("Fallback (Doğrudan Çıkış)") Altyapısı.

pub mod mmnotify;

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;

use ozan_shm::{
    OzanSharedBlock, SharedMemoryMapping, FLAG_APO_ACTIVE, FLAG_CHAINING_ACTIVE,
    OZAN_APO_CLSID, OZAN_APO_CLSID_STR,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE, S_OK};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_MULTI_SZ,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};

use mmnotify::{query_default_capture_endpoint, CaptureEndpointInfo};

/// Windows Ses Motoru `FxProperties` Kayıt Defteri Özellik Anahtarları
/// (`_PKEY_FX_StreamEffectClsid`, `_PKEY_FX_ModeEffectClsid`, `_PKEY_CompositeFX_*`)
pub const PKEY_FX_STREAM_EFFECT: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},1";
pub const PKEY_FX_MODE_EFFECT: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},2";
pub const PKEY_COMPOSITE_FX_STREAM_EFFECT: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},5";
pub const PKEY_COMPOSITE_FX_MODE_EFFECT: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},6";

/// Ozan tarafından `FxProperties` içinde saklanan yedek anahtar isimleri
pub const OZAN_REG_BACKUP_SFX: &str = "OzanBackup_SFX";
pub const OZAN_REG_BACKUP_MFX: &str = "OzanBackup_MFX";
pub const OZAN_REG_CHAINED_CLSID: &str = "OzanChainedClsid";

/// Ses enjeksiyon çalışma modu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InjectionMode {
    /// `ozan-apo.dll` aktif mikrofona zincirlenmiş durumda; doğrudan mikrofona enjekte ediyor.
    ApoActive,
    /// Güvenlik politikası, yetki kısıtı veya sıfırlanmış kayıt nedeniyle
    /// otomatik olarak "Fallback (Doğrudan Çıkış)" moduna geçildi.
    FallbackDirectOutput,
}

/// Bir mikrofon uç noktasının (Endpoint) orijinal üretici `FxProperties` yedeği.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointFxBackup {
    pub endpoint_guid: String,
    pub endpoint_name: String,
    pub original_sfx: Option<String>,
    pub original_mfx: Option<String>,
    pub original_composite_sfx: Vec<String>,
    pub original_composite_mfx: Vec<String>,
}

/// Tüm uç noktaların yerel JSON yedeği (`%APPDATA%\Ozan\apo-backup.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApoBackupStore {
    pub expected_dll_sha256: Option<String>,
    pub endpoints: HashMap<String, EndpointFxBackup>,
}

/// Arayüzde ve sağlık denetiminde kullanılan kapsamlı APO durum raporu.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApoHealthReport {
    pub mode: InjectionMode,
    pub mode_label: String,
    pub dll_path: String,
    pub dll_exists: bool,
    pub dll_loadable: bool,
    pub dll_sha256: String,
    pub expected_sha256: String,
    pub sha256_valid: bool,
    pub com_registered: bool,
    pub fx_chain_intact: bool,
    pub active_endpoint_id: Option<String>,
    pub active_endpoint_name: Option<String>,
    pub original_chained_clsids: Vec<String>,
    pub shm_connected: bool,
    pub shm_namespace: String,
    pub apo_sample_rate: u32,
    pub apo_channels: u32,
    pub apo_heartbeat: u64,
    pub needs_repair: bool,
    pub status_message: String,
    pub fallback_reason: Option<String>,
}

/// Bir dosyanın SHA-256 özetini küçük harf hex dizgesi olarak hesaplar.
pub fn compute_file_sha256(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("Dosya okunamadı ({}): {e}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// `ozan.exe` dosyasının bulunduğu klasördeki (yan yana) `ozan-apo.dll` yolunu döndürür.
pub fn exe_adjacent_apo_dll_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some(dir.join("ozan-apo.dll"))
}

/// Sistemdeki kaynak `ozan-apo.dll` (veya `ozan_apo.dll`) dosyasını `ozan.exe`'nin
/// bulunduğu dizinden (göreceli olarak) bulur.
pub fn locate_source_apo_dll() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("ozan-apo.dll"));
            candidates.push(dir.join("ozan_apo.dll"));
            candidates.push(dir.join("resources").join("ozan-apo.dll"));
            // Test çalıştırıcısı target/<profile>/deps/ altındaysa bir üst dizine de bak
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join("ozan-apo.dll"));
                candidates.push(parent.join("ozan_apo.dll"));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("ozan-apo.dll"));
        candidates.push(cwd.join("ozan_apo.dll"));
    }

    candidates.into_iter().find(|p| p.is_file())
}

/// Çalışma zamanında kullanılacak birincil `ozan-apo.dll` yolunu döndürür:
/// Öncelikle `ozan.exe` ile aynı klasördeki (`<exe_dir>\ozan-apo.dll`) yolu kullanır;
/// eğer `ozan.exe` dizinine yazma izni yoksa veya dosya orada değilse `%APPDATA%\Ozan\bin\ozan-apo.dll` yedek yolunu döner.
pub fn runtime_apo_dll_path(app_data_dir: &Path) -> PathBuf {
    if let Some(adj) = exe_adjacent_apo_dll_path() {
        if adj.is_file() {
            return adj;
        }
    }
    app_data_dir.join("bin").join("ozan-apo.dll")
}

/// `%APPDATA%\Ozan\bin\ozan-apo.dll` Self-Healing yedek yansıma (mirror) yolu.
pub fn mirror_apo_dll_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("bin").join("ozan-apo.dll")
}

/// `%APPDATA%\Ozan\apo-backup.json` yedek dosyasını okur.
pub fn load_backup_store(app_data_dir: &Path) -> ApoBackupStore {
    let path = app_data_dir.join("apo-backup.json");
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// `%APPDATA%\Ozan\apo-backup.json` yedek dosyasını kaydeder.
pub fn save_backup_store(app_data_dir: &Path, store: &ApoBackupStore) -> Result<(), String> {
    let _ = fs::create_dir_all(app_data_dir);
    let path = app_data_dir.join("apo-backup.json");
    let json = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| format!("Yedek dosyası yazılamadı: {e}"))
}

/// `ozan-apo.dll` dosyasının güvenlik yazılımı / sistem politikası tarafından engellenmeden
/// yüklenebildiğini ve `DllGetClassObject` COM giriş noktasının çalıştığını güvenli biçimde doğrular.
pub fn verify_dll_loadable(dll_path: &Path) -> Result<(), String> {
    if !dll_path.is_file() {
        return Err("ozan-apo.dll dosyası bulunamadı.".into());
    }
    unsafe {
        let wide: Vec<u16> = dll_path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hmod: HMODULE = LoadLibraryW(wide.as_ptr());
        if hmod.is_null() {
            return Err(
                "Güvenlik yazılımı veya sistem politikası ozan-apo.dll yüklenmesini engelledi."
                    .into(),
            );
        }

        type DllGetClassObjectFn =
            unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
        let proc = GetProcAddress(hmod, b"DllGetClassObject\0".as_ptr());
        if proc.is_none() {
            FreeLibrary(hmod);
            return Err("ozan-apo.dll içinde DllGetClassObject COM ihracı bulunamadı.".into());
        }

        let func: DllGetClassObjectFn = std::mem::transmute(proc.unwrap());
        let iid_unk = GUID {
            data1: 0x0000_0000,
            data2: 0x0000,
            data3: 0x0000,
            data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
        };
        let mut factory_unk: *mut c_void = null_mut();
        let hr = func(&OZAN_APO_CLSID, &iid_unk, &mut factory_unk);
        if hr == S_OK && !factory_unk.is_null() {
            #[repr(C)]
            struct UnkVtbl {
                qi: usize,
                add_ref: usize,
                release: unsafe extern "system" fn(*mut c_void) -> u32,
            }
            let vtbl = *(factory_unk as *const *const UnkVtbl);
            ((*vtbl).release)(factory_unk);
        }
        FreeLibrary(hmod);
        if hr != S_OK {
            return Err(format!("DllGetClassObject doğrulaması başarısız (HRESULT: 0x{hr:08X})."));
        }
    }
    Ok(())
}

// ============================================================================
// Windows Kayıt Defteri (Registry) Yardımcıları
// ============================================================================

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn reg_read_sz(hkey: HKEY, name: &str) -> Option<String> {
    let wname = to_wide(name);
    let mut dtype = 0u32;
    let mut size = 0u32;
    if RegQueryValueExW(
        hkey,
        if name.is_empty() { null() } else { wname.as_ptr() },
        null(),
        &mut dtype,
        null_mut(),
        &mut size,
    ) != 0
        || dtype != REG_SZ
        || size < 2
    {
        return None;
    }
    let mut buf = vec![0u16; (size as usize + 1) / 2];
    if RegQueryValueExW(
        hkey,
        if name.is_empty() { null() } else { wname.as_ptr() },
        null(),
        &mut dtype,
        buf.as_mut_ptr() as *mut u8,
        &mut size,
    ) != 0
    {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..len]).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

unsafe fn reg_read_multi_sz(hkey: HKEY, name: &str) -> Vec<String> {
    let wname = to_wide(name);
    let mut dtype = 0u32;
    let mut size = 0u32;
    if RegQueryValueExW(
        hkey,
        wname.as_ptr(),
        null(),
        &mut dtype,
        null_mut(),
        &mut size,
    ) != 0
        || dtype != REG_MULTI_SZ
        || size < 2
    {
        return Vec::new();
    }
    let mut buf = vec![0u16; (size as usize + 1) / 2];
    if RegQueryValueExW(
        hkey,
        wname.as_ptr(),
        null(),
        &mut dtype,
        buf.as_mut_ptr() as *mut u8,
        &mut size,
    ) != 0
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    for i in 0..buf.len() {
        if buf[i] == 0 {
            if i > start {
                let s = String::from_utf16_lossy(&buf[start..i]).trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
            }
            start = i + 1;
        }
    }
    out
}

unsafe fn reg_write_sz(hkey: HKEY, name: &str, value: &str) -> bool {
    let wname = to_wide(name);
    let wval = to_wide(value);
    RegSetValueExW(
        hkey,
        if name.is_empty() { null() } else { wname.as_ptr() },
        0,
        REG_SZ,
        wval.as_ptr() as *const u8,
        (wval.len() * 2) as u32,
    ) == 0
}

unsafe fn reg_write_multi_sz(hkey: HKEY, name: &str, values: &[String]) -> bool {
    let wname = to_wide(name);
    let mut buf: Vec<u16> = Vec::new();
    for v in values {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            buf.extend(trimmed.encode_utf16());
            buf.push(0);
        }
    }
    buf.push(0);
    RegSetValueExW(
        hkey,
        wname.as_ptr(),
        0,
        REG_MULTI_SZ,
        buf.as_ptr() as *const u8,
        (buf.len() * 2) as u32,
    ) == 0
}

/// COM CLSID (`{8A4E7B21-6C3D-4F92-B8E1-5D2A9F0C4E73}`) kaydının mevcut olup olmadığını kontrol eder.
pub fn is_com_clsid_registered(expected_dll: &Path) -> bool {
    let subkey = format!("SOFTWARE\\Classes\\CLSID\\{OZAN_APO_CLSID_STR}\\InprocServer32");
    let wsub = to_wide(&subkey);
    let expected_norm = expected_dll.to_string_lossy().to_lowercase();

    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        unsafe {
            let mut hkey: HKEY = null_mut();
            if RegOpenKeyExW(root, wsub.as_ptr(), 0, KEY_READ, &mut hkey) == 0 {
                let val = reg_read_sz(hkey, "");
                RegCloseKey(hkey);
                if let Some(v) = val {
                    if v.to_lowercase() == expected_norm || Path::new(&v).is_file() {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// COM CLSID kaydını (`HKLM` öncelikli, yetki yoksa `HKCU` yedekli) yazar.
pub fn register_com_clsid(dll_path: &Path) -> Result<(), String> {
    let subkey = format!("SOFTWARE\\Classes\\CLSID\\{OZAN_APO_CLSID_STR}\\InprocServer32");
    let wsub = to_wide(&subkey);
    let dll_str = dll_path.to_string_lossy().to_string();

    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        unsafe {
            let mut hkey: HKEY = null_mut();
            if RegCreateKeyExW(
                root,
                wsub.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                null(),
                &mut hkey,
                null_mut(),
            ) == 0
            {
                let ok1 = reg_write_sz(hkey, "", &dll_str);
                let ok2 = reg_write_sz(hkey, "ThreadingModel", "Both");
                RegCloseKey(hkey);
                if ok1 && ok2 {
                    return Ok(());
                }
            }
        }
    }
    Err("COM CLSID kayıt defterine yazılamadı.".into())
}

/// Aktif mikrofonun `FxProperties` anahtarını inceler; mevcut üretici APO CLSID'lerini
/// ve Ozan APO zincirinin (`OZAN_APO_CLSID_STR`) durumunu döndürür.
pub fn inspect_endpoint_fx(
    endpoint_guid: &str,
    backup_store: &ApoBackupStore,
) -> (bool, Vec<String>) {
    let mut original_clsids: Vec<String> = Vec::new();
    let mut ozan_chained = false;

    if let Some(b) = backup_store.endpoints.get(endpoint_guid) {
        if let Some(sfx) = &b.original_sfx {
            if !sfx.eq_ignore_ascii_case(OZAN_APO_CLSID_STR) && !original_clsids.contains(sfx) {
                original_clsids.push(sfx.clone());
            }
        }
        for c in &b.original_composite_sfx {
            if !c.eq_ignore_ascii_case(OZAN_APO_CLSID_STR) && !original_clsids.contains(c) {
                original_clsids.push(c.clone());
            }
        }
    }

    let fx_subkey = format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\MMDevices\\Audio\\Capture\\{endpoint_guid}\\FxProperties"
    );
    let wfx = to_wide(&fx_subkey);

    // Hem HKLM (gerçek MMDevices) hem de HKCU (kullanıcı düzeyinde yedek/ayna) kontrol et
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        unsafe {
            let mut hkey: HKEY = null_mut();
            if RegOpenKeyExW(root, wfx.as_ptr(), 0, KEY_READ, &mut hkey) == 0 {
                if let Some(sfx) = reg_read_sz(hkey, PKEY_FX_STREAM_EFFECT) {
                    if sfx.eq_ignore_ascii_case(OZAN_APO_CLSID_STR) {
                        ozan_chained = true;
                    } else if !original_clsids.contains(&sfx) {
                        original_clsids.push(sfx);
                    }
                }
                if let Some(mfx) = reg_read_sz(hkey, PKEY_FX_MODE_EFFECT) {
                    if mfx.eq_ignore_ascii_case(OZAN_APO_CLSID_STR) {
                        ozan_chained = true;
                    } else if !original_clsids.contains(&mfx) {
                        original_clsids.push(mfx);
                    }
                }
                for c in reg_read_multi_sz(hkey, PKEY_COMPOSITE_FX_STREAM_EFFECT) {
                    if c.eq_ignore_ascii_case(OZAN_APO_CLSID_STR) {
                        ozan_chained = true;
                    } else if !original_clsids.contains(&c) {
                        original_clsids.push(c);
                    }
                }
                if let Some(chained_bak) = reg_read_sz(hkey, OZAN_REG_CHAINED_CLSID) {
                    if !chained_bak.eq_ignore_ascii_case(OZAN_APO_CLSID_STR)
                        && !original_clsids.contains(&chained_bak)
                    {
                        original_clsids.push(chained_bak);
                    }
                }
                RegCloseKey(hkey);
            }
        }
    }

    (ozan_chained, original_clsids)
}

/// Aktif mikrofonun `FxProperties` kayıtlarında mevcut üretici APO CLSID'lerini
/// SİLMEDEN yedekler ve `ozan-apo.dll` bileşenini APO zincirine ekler (APO Chaining).
pub fn chain_apo_on_endpoint(
    app_data_dir: &Path,
    endpoint: &CaptureEndpointInfo,
    shm: Option<&OzanSharedBlock>,
) -> Result<Vec<String>, String> {
    let mut store = load_backup_store(app_data_dir);
    let guid = &endpoint.endpoint_guid;

    let fx_subkey = format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\MMDevices\\Audio\\Capture\\{guid}\\FxProperties"
    );
    let wfx = to_wide(&fx_subkey);

    // 1. Mevcut üretici APO değerlerini oku ve henüz yedeklenmemişse yedekle
    unsafe {
        let mut hkey_read: HKEY = null_mut();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, wfx.as_ptr(), 0, KEY_READ, &mut hkey_read) == 0 {
            let cur_sfx = reg_read_sz(hkey_read, PKEY_FX_STREAM_EFFECT);
            let cur_mfx = reg_read_sz(hkey_read, PKEY_FX_MODE_EFFECT);
            let cur_comp_sfx = reg_read_multi_sz(hkey_read, PKEY_COMPOSITE_FX_STREAM_EFFECT);
            let cur_comp_mfx = reg_read_multi_sz(hkey_read, PKEY_COMPOSITE_FX_MODE_EFFECT);
            RegCloseKey(hkey_read);

            let entry = store
                .endpoints
                .entry(guid.clone())
                .or_insert_with(|| EndpointFxBackup {
                    endpoint_guid: guid.clone(),
                    endpoint_name: endpoint.friendly_name.clone(),
                    ..Default::default()
                });

            if let Some(s) = cur_sfx {
                if !s.eq_ignore_ascii_case(OZAN_APO_CLSID_STR) && entry.original_sfx.is_none() {
                    entry.original_sfx = Some(s);
                }
            }
            if let Some(m) = cur_mfx {
                if !m.eq_ignore_ascii_case(OZAN_APO_CLSID_STR) && entry.original_mfx.is_none() {
                    entry.original_mfx = Some(m);
                }
            }
            for c in cur_comp_sfx {
                if !c.eq_ignore_ascii_case(OZAN_APO_CLSID_STR)
                    && !entry.original_composite_sfx.contains(&c)
                {
                    entry.original_composite_sfx.push(c);
                }
            }
            for c in cur_comp_mfx {
                if !c.eq_ignore_ascii_case(OZAN_APO_CLSID_STR)
                    && !entry.original_composite_mfx.contains(&c)
                {
                    entry.original_composite_mfx.push(c);
                }
            }
        }
    }

    let backup = store
        .endpoints
        .entry(guid.clone())
        .or_insert_with(|| EndpointFxBackup {
            endpoint_guid: guid.clone(),
            endpoint_name: endpoint.friendly_name.clone(),
            ..Default::default()
        })
        .clone();
    let _ = save_backup_store(app_data_dir, &store);

    // Orijinal üretici CLSID listesini derle
    let mut chained_originals: Vec<String> = Vec::new();
    if let Some(orig_sfx) = &backup.original_sfx {
        if !orig_sfx.eq_ignore_ascii_case(OZAN_APO_CLSID_STR) {
            chained_originals.push(orig_sfx.clone());
        }
    }
    for c in &backup.original_composite_sfx {
        if !c.eq_ignore_ascii_case(OZAN_APO_CLSID_STR) && !chained_originals.contains(c) {
            chained_originals.push(c.clone());
        }
    }

    // Paylaşımlı belleğe zincirlenen birincil üretici CLSID'sini bildir
    if let Some(block) = shm {
        block.set_chained_clsid(chained_originals.first().map(|s| s.as_str()));
    }

    // Composite SFX zinciri: [Orijinal Üretici APO'ları..., OZAN_APO_CLSID]
    let mut composite_chain = chained_originals.clone();
    if !composite_chain
        .iter()
        .any(|c| c.eq_ignore_ascii_case(OZAN_APO_CLSID_STR))
    {
        composite_chain.push(OZAN_APO_CLSID_STR.to_string());
    }

    // 2. FxProperties'e yazmayı dene (Önce HKLM, standart kullanıcıda erişim engellenirse HKCU aynası)
    let mut wrote_any = false;
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        unsafe {
            let mut hkey: HKEY = null_mut();
            if RegCreateKeyExW(
                root,
                wfx.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                null(),
                &mut hkey,
                null_mut(),
            ) == 0
            {
                if let Some(orig_sfx) = &backup.original_sfx {
                    let _ = reg_write_sz(hkey, OZAN_REG_BACKUP_SFX, orig_sfx);
                    let _ = reg_write_sz(hkey, OZAN_REG_CHAINED_CLSID, orig_sfx);
                }
                if let Some(orig_mfx) = &backup.original_mfx {
                    let _ = reg_write_sz(hkey, OZAN_REG_BACKUP_MFX, orig_mfx);
                }
                let ok_sfx = reg_write_sz(hkey, PKEY_FX_STREAM_EFFECT, OZAN_APO_CLSID_STR);
                let ok_comp =
                    reg_write_multi_sz(hkey, PKEY_COMPOSITE_FX_STREAM_EFFECT, &composite_chain);
                RegCloseKey(hkey);
                if ok_sfx && ok_comp {
                    wrote_any = true;
                    if root == HKEY_LOCAL_MACHINE {
                        break;
                    }
                }
            }
        }
    }

    if wrote_any {
        Ok(chained_originals)
    } else {
        Err("FxProperties kayıt defteri anahtarına yazma izni reddedildi.".into())
    }
}

// ============================================================================
// Ana `ApoManager` (Self-Healing, SHA-256 Bütünlük ve Fallback Yönetimi)
// ============================================================================

pub struct ApoManager {
    pub app_data_dir: PathBuf,
    pub shm: Option<SharedMemoryMapping>,
}

impl ApoManager {
    /// Uygulama açılışında APO altyapısını başlatır, `ozan-apo.dll` SHA-256 bütünlüğünü
    /// ve `FxProperties` zincirini otomatik denetler.
    pub fn init(app_data_dir: PathBuf) -> Self {
        let shm = SharedMemoryMapping::create_or_open().ok();
        let mgr = Self { app_data_dir, shm };
        // İlk açılışta ozan-apo.dll kopyasını ve COM/FxProperties kaydını sessizce hazırla
        let _ = mgr.ensure_runtime_dll();
        let _ = mgr.auto_heal_if_possible();
        mgr
    }

    pub fn shared_block(&self) -> Option<&OzanSharedBlock> {
        self.shm.as_ref().map(|m| m.block())
    }

    /// `ozan.exe` yanındaki `ozan-apo.dll` ile `%APPDATA%\Ozan\bin\ozan-apo.dll`
    /// yansımasını (mirror) senkronize eder ve beklenen SHA-256 özetini günceller.
    pub fn ensure_runtime_dll(&self) -> Result<(PathBuf, String), String> {
        let mirror = mirror_apo_dll_path(&self.app_data_dir);
        if let Some(parent) = mirror.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let mut store = load_backup_store(&self.app_data_dir);

        if let Some(src) = locate_source_apo_dll() {
            let src_hash = compute_file_sha256(&src)?;
            // Mirror kopyasını da güncel tut (Self-Healing yedeği)
            if src != mirror {
                let need_mirror = match compute_file_sha256(&mirror) {
                    Ok(mh) => mh != src_hash,
                    Err(_) => true,
                };
                if need_mirror {
                    let _ = fs::copy(&src, &mirror);
                }
            }
            // Eğer ozan.exe'nin yanında ozan-apo.dll yoksa ve yazılabilirse yanına da kopyala
            if let Some(adj) = exe_adjacent_apo_dll_path() {
                if !adj.is_file() && src != adj {
                    let _ = fs::copy(&src, &adj);
                }
            }
            if store.expected_dll_sha256.as_deref() != Some(src_hash.as_str()) {
                store.expected_dll_sha256 = Some(src_hash.clone());
                let _ = save_backup_store(&self.app_data_dir, &store);
            }
            let active_path = runtime_apo_dll_path(&self.app_data_dir);
            return Ok((active_path, src_hash));
        }

        // Eğer yan yana DLL silinmişse ancak mirror duruyorsa yan yana geri yükle (Self-Heal)
        if mirror.is_file() {
            if let Some(adj) = exe_adjacent_apo_dll_path() {
                if !adj.is_file() {
                    let _ = fs::copy(&mirror, &adj);
                }
            }
            let h = compute_file_sha256(&mirror)?;
            let expected = store.expected_dll_sha256.unwrap_or_else(|| h.clone());
            return Ok((runtime_apo_dll_path(&self.app_data_dir), expected));
        }

        Err("ozan-apo.dll bulunamadı.".into())
    }

    /// Açılışta veya cihaz değişiminde sessiz onarım dener; hata olursa asla çökmez.
    pub fn auto_heal_if_possible(&self) -> ApoHealthReport {
        if let Ok((dll_path, _)) = self.ensure_runtime_dll() {
            if verify_dll_loadable(&dll_path).is_ok() {
                let _ = register_com_clsid(&dll_path);
                if let Some(ep) = query_default_capture_endpoint() {
                    if let Some(block) = self.shared_block() {
                        if block.apo_sample_rate.load(Ordering::Relaxed) == 0 {
                            block.apo_sample_rate.store(ep.sample_rate, Ordering::Release);
                            block.apo_channels.store(ep.channels, Ordering::Release);
                        }
                    }
                    let _ = chain_apo_on_endpoint(&self.app_data_dir, &ep, self.shared_block());
                }
            }
        }
        self.inspect_health()
    }

    /// Kullanıcı "Tek Tıkla Onar (Self-Heal)" butonuna bastığında çalıştırılır:
    /// 1. `ozan-apo.dll` dosyasını kaynak kopyadan onarır ve SHA-256 özetini doğrular.
    /// 2. DLL'in yüklenebilirliğini (`DllGetClassObject`) test eder.
    /// 3. COM CLSID kaydını ve aktif mikrofonun `FxProperties` APO zincirini (orijinal
    ///    üretici efektlerini koruyarak) yeniden kurar.
    pub fn repair(&self) -> ApoHealthReport {
        // 1. Kaynak veya yansıma (mirror) DLL'den zorunlu yenile
        let mirror = mirror_apo_dll_path(&self.app_data_dir);
        if let Some(p) = mirror.parent() {
            let _ = fs::create_dir_all(p);
        }
        if let Some(src) = locate_source_apo_dll() {
            if src != mirror {
                let _ = fs::copy(&src, &mirror);
            }
            if let Some(adj) = exe_adjacent_apo_dll_path() {
                if src != adj {
                    let _ = fs::copy(&src, &adj);
                }
            }
            if let Ok(h) = compute_file_sha256(&src) {
                let mut store = load_backup_store(&self.app_data_dir);
                store.expected_dll_sha256 = Some(h);
                let _ = save_backup_store(&self.app_data_dir, &store);
            }
        } else if mirror.is_file() {
            if let Some(adj) = exe_adjacent_apo_dll_path() {
                let _ = fs::copy(&mirror, &adj);
            }
        }

        let dst = runtime_apo_dll_path(&self.app_data_dir);
        if verify_dll_loadable(&dst).is_ok() {
            let _ = register_com_clsid(&dst);
            if let Some(ep) = query_default_capture_endpoint() {
                let _ = chain_apo_on_endpoint(&self.app_data_dir, &ep, self.shared_block());
            }
        }

        self.inspect_health()
    }

    /// Tam kapsamlı APO bütünlük, `FxProperties` zincir ve paylaşımlı bellek denetimi yapar.
    pub fn inspect_health(&self) -> ApoHealthReport {
        let store = load_backup_store(&self.app_data_dir);
        let dst = runtime_apo_dll_path(&self.app_data_dir);
        let dll_path_str = dst.to_string_lossy().to_string();
        let dll_exists = dst.is_file();

        let expected_sha256 = store
            .expected_dll_sha256
            .clone()
            .or_else(|| locate_source_apo_dll().and_then(|p| compute_file_sha256(&p).ok()))
            .unwrap_or_default();

        let dll_sha256 = if dll_exists {
            compute_file_sha256(&dst).unwrap_or_default()
        } else {
            String::new()
        };

        let sha256_valid =
            dll_exists && !dll_sha256.is_empty() && (expected_sha256.is_empty() || dll_sha256 == expected_sha256);

        let load_check = if dll_exists {
            verify_dll_loadable(&dst)
        } else {
            Err("ozan-apo.dll mevcut değil.".into())
        };
        let dll_loadable = load_check.is_ok();

        let com_registered = dll_exists && is_com_clsid_registered(&dst);

        let endpoint = query_default_capture_endpoint();
        let (fx_chain_intact, original_chained_clsids) = match &endpoint {
            Some(ep) => inspect_endpoint_fx(&ep.endpoint_guid, &store),
            None => (false, Vec::new()),
        };

        let (shm_connected, shm_namespace, apo_sample_rate, apo_channels, apo_heartbeat, apo_flag_active) =
            match &self.shm {
                Some(m) => {
                    let b = m.block();
                    let rate = b.apo_sample_rate.load(Ordering::Relaxed);
                    let ch = b.apo_channels.load(Ordering::Relaxed);
                    let hb = b.apo_heartbeat.load(Ordering::Relaxed);
                    let fl = b.flags.load(Ordering::Relaxed);
                    (
                        true,
                        m.namespace().to_string(),
                        if rate > 0 {
                            rate
                        } else {
                            endpoint.as_ref().map(|e| e.sample_rate).unwrap_or(48_000)
                        },
                        if ch > 0 {
                            ch
                        } else {
                            endpoint.as_ref().map(|e| e.channels).unwrap_or(2)
                        },
                        hb,
                        (fl & FLAG_APO_ACTIVE) != 0,
                    )
                }
                None => (false, String::from("Yok"), 48_000, 2, 0, false),
            };

        if let Some(b) = self.shared_block() {
            if !original_chained_clsids.is_empty() {
                b.set_chained_clsid(original_chained_clsids.first().map(|s| s.as_str()));
            } else {
                b.flags.fetch_and(!FLAG_CHAINING_ACTIVE, Ordering::Relaxed);
            }
        }

        // Mod ve onarım gereksinimi kararı
        let mut fallback_reason: Option<String> = None;
        let mut needs_repair = false;

        if !dll_exists || !sha256_valid {
            fallback_reason = Some(
                "ozan-apo.dll dosyası eksik veya SHA-256 bütünlük doğrulaması başarısız oldu."
                    .into(),
            );
            needs_repair = true;
        } else if let Err(load_err) = load_check {
            fallback_reason = Some(load_err);
            needs_repair = true;
        } else if !com_registered || !fx_chain_intact {
            fallback_reason = Some(
                "Windows Kayıt Defteri (FxProperties) APO zinciri sıfırlanmış veya kayıtlı değil."
                    .into(),
            );
            needs_repair = true;
        } else if !shm_connected {
            fallback_reason = Some("Paylaşımlı bellek (Shared Memory) bağlantısı kurulamadı.".into());
            needs_repair = true;
        }

        let mode = if fallback_reason.is_none() {
            InjectionMode::ApoActive
        } else {
            InjectionMode::FallbackDirectOutput
        };

        let mode_label = match mode {
            InjectionMode::ApoActive => {
                if apo_flag_active || apo_heartbeat > 0 {
                    "APO Aktif (audiodg.exe Canlı)".to_string()
                } else {
                    "APO Hazır (Zincirli)".to_string()
                }
            }
            InjectionMode::FallbackDirectOutput => "Fallback (Doğrudan Çıkış)".to_string(),
        };

        let status_message = match mode {
            InjectionMode::ApoActive => {
                let ep_name = endpoint
                    .as_ref()
                    .map(|e| e.friendly_name.as_str())
                    .unwrap_or("Varsayılan Mikrofon");
                let chain_info = if original_chained_clsids.is_empty() {
                    "Doğrudan APO".to_string()
                } else {
                    format!("{} üretici APO korundu", original_chained_clsids.len())
                };
                format!(
                    "{ep_name} ({apo_sample_rate} Hz, {apo_channels} Kanal) — {chain_info}"
                )
            }
            InjectionMode::FallbackDirectOutput => fallback_reason
                .clone()
                .unwrap_or_else(|| "Fallback (Doğrudan Çıkış) modu aktif.".into()),
        };

        ApoHealthReport {
            mode,
            mode_label,
            dll_path: dll_path_str,
            dll_exists,
            dll_loadable,
            dll_sha256,
            expected_sha256,
            sha256_valid,
            com_registered,
            fx_chain_intact,
            active_endpoint_id: endpoint.as_ref().map(|e| e.endpoint_guid.clone()),
            active_endpoint_name: endpoint.as_ref().map(|e| e.friendly_name.clone()),
            original_chained_clsids,
            shm_connected,
            shm_namespace,
            apo_sample_rate,
            apo_channels,
            apo_heartbeat,
            needs_repair,
            status_message,
            fallback_reason,
        }
    }
}
