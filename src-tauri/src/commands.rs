//! Arayüzden çağrılan Tauri komutları.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::apo::{ApoHealthReport, ApoManager};
use crate::audio::{self, AudioEngine, AudioStatus, DeviceList};
use crate::backup;
use crate::input;
use crate::library::{
    self, apply_order, display_name, is_supported, Folder, HotkeyBinding, Library, LibraryData,
    ReplayMode, Sound,
};
use crate::trimmer::{self, WaveformData};

/// -1 dBFS tepe hedefi (doğrusal).
const TARGET_PEAK: f32 = 0.891_250_9; // 10^(-1/20)

pub struct AppState {
    pub lib: Mutex<Library>,
    pub engine: AudioEngine,
    pub apo: ApoManager,
}

impl AppState {
    pub fn lib(&self) -> MutexGuard<'_, Library> {
        self.lib.lock().unwrap_or_else(|p| p.into_inner())
    }
}

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub fn get_library(state: State<'_, AppState>) -> LibraryData {
    state.lib().data.clone()
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImportProgress {
    done: usize,
    total: usize,
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    added: Vec<Sound>,
    errors: Vec<String>,
    skipped: usize,
}

#[tauri::command]
pub async fn import_files(
    app: AppHandle,
    paths: Vec<String>,
    normalize: bool,
    remember: bool,
) -> CmdResult<ImportResult> {
    tauri::async_runtime::spawn_blocking(move || import_blocking(&app, paths, normalize, remember))
        .await
        .map_err(|e| format!("İçe aktarma görevi başarısız: {e}"))?
}

fn import_blocking(
    app: &AppHandle,
    paths: Vec<String>,
    normalize: bool,
    remember: bool,
) -> CmdResult<ImportResult> {
    let state = app.state::<AppState>();
    let sounds_dir = {
        let mut lib = state.lib();
        if remember {
            lib.data.settings.normalize_choice = Some(normalize);
            lib.save()?;
        }
        lib.sounds_dir.clone()
    };

    let files: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    let valid: Vec<&PathBuf> = files.iter().filter(|p| p.is_file() && is_supported(p)).collect();
    let skipped = files.len() - valid.len();
    let total = valid.len();
    let mut added = Vec::new();
    let mut errors = Vec::new();

    for (i, src) in valid.into_iter().enumerate() {
        let name = display_name(src);
        let _ = app.emit(
            "import-progress",
            ImportProgress {
                done: i,
                total,
                name: name.clone(),
            },
        );
        match import_one(&sounds_dir, src, normalize) {
            Ok(sound) => {
                let mut lib = state.lib();
                lib.data.sounds.push(sound.clone());
                if let Err(e) = lib.save() {
                    errors.push(e);
                }
                added.push(sound);
            }
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    let _ = app.emit(
        "import-progress",
        ImportProgress {
            done: total,
            total,
            name: String::new(),
        },
    );
    Ok(ImportResult {
        added,
        errors,
        skipped,
    })
}

fn import_one(sounds_dir: &Path, src: &Path, normalize: bool) -> CmdResult<Sound> {
    let dest = library::copy_into(sounds_dir, src)?;
    let result = (|| {
        let (gain, duration) = if normalize {
            let (peak, duration) = audio::analyze(&dest)?;
            let gain = if peak > 1e-6 { TARGET_PEAK / peak } else { 1.0 };
            (gain, duration)
        } else {
            audio::source::open_source(&dest)?;
            (1.0, audio::probe_duration(&dest))
        };
        Ok::<_, String>(Sound {
            id: library::new_id("s"),
            name: display_name(src),
            file: dest.file_name().unwrap_or_default().to_string_lossy().to_string(),
            gain,
            user_gain: 1.0,
            normalized: normalize,
            favorite: false,
            duration,
            color: String::new(),
            replay_mode: ReplayMode::Restart,
            hotkey: None,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&dest);
    }
    result
}

#[tauri::command]
pub fn delete_sounds(state: State<'_, AppState>, ids: Vec<String>) -> CmdResult<LibraryData> {
    state.engine.stop_all();
    let mut lib = state.lib();
    let errors = lib.delete_sounds(&ids);
    lib.save()?;
    drop(lib);
    input::sync_from_library();
    if !errors.is_empty() {
        return Err(format!("Bazı dosyalar silinemedi: {}", errors.join("; ")));
    }
    Ok(state.lib().data.clone())
}

#[tauri::command]
pub fn rename_sound(state: State<'_, AppState>, id: String, name: String) -> CmdResult<()> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("İsim boş olamaz".into());
    }
    let mut lib = state.lib();
    lib.sound_mut(&id)?.name = name;
    lib.save()
}

#[tauri::command]
pub fn set_favorite(state: State<'_, AppState>, id: String, favorite: bool) -> CmdResult<()> {
    let mut lib = state.lib();
    lib.sound_mut(&id)?.favorite = favorite;
    lib.save()
}

/// Ses kartı bireysel ses kazancını (%0 - %200 -> 0.0 ..= 2.0) ayarlar.
#[tauri::command]
pub fn set_sound_gain(
    state: State<'_, AppState>,
    id: String,
    user_gain: f32,
    persist: bool,
) -> CmdResult<()> {
    let ug = user_gain.clamp(0.0, 2.0);
    let mut lib = state.lib();
    let s = lib.sound_mut(&id)?;
    s.user_gain = ug;
    let eff = s.effective_gain();
    state.engine.set_voice_gain(id, eff);
    if persist {
        lib.save()?;
    }
    Ok(())
}

/// Ses kartı özel renk etiketini ayarlar.
#[tauri::command]
pub fn set_sound_color(state: State<'_, AppState>, id: String, color: String) -> CmdResult<()> {
    let mut lib = state.lib();
    lib.sound_mut(&id)?.color = color.trim().to_string();
    lib.save()
}

/// Ses kartı tekrar modunu (`"restart"` veya `"overlap"`) ayarlar.
#[tauri::command]
pub fn set_sound_replay_mode(
    state: State<'_, AppState>,
    id: String,
    replay_mode: ReplayMode,
) -> CmdResult<()> {
    let mut lib = state.lib();
    lib.sound_mut(&id)?.replay_mode = replay_mode;
    lib.save()
}

/// Ses kartı kısayolunu ve Passthrough seçeneğini ayarlar.
#[tauri::command]
pub fn set_sound_hotkey(
    state: State<'_, AppState>,
    id: String,
    hotkey: Option<HotkeyBinding>,
) -> CmdResult<()> {
    {
        let mut lib = state.lib();
        lib.sound_mut(&id)?.hotkey = hotkey.filter(|h| !h.code.is_empty());
        lib.save()?;
    }
    input::sync_from_library();
    Ok(())
}

#[tauri::command]
pub fn start_hotkey_capture() {
    input::start_capture();
}

#[tauri::command]
pub fn cancel_hotkey_capture() {
    input::cancel_capture();
}

/// Acil durdurma (Panic) kısayolunu günceller.
#[tauri::command]
pub fn set_panic_hotkey(state: State<'_, AppState>, hotkey: HotkeyBinding) -> CmdResult<()> {
    if hotkey.code.is_empty() {
        return Err("Panic tuşu boş olamaz".into());
    }
    {
        let mut lib = state.lib();
        lib.data.settings.panic_hotkey = hotkey;
        lib.save()?;
    }
    input::sync_from_library();
    Ok(())
}

/// Bas-Konuş (Push-to-Talk) ayarlarını günceller.
#[tauri::command]
pub fn set_ptt_config(
    state: State<'_, AppState>,
    enabled: bool,
    hotkey: Option<HotkeyBinding>,
) -> CmdResult<()> {
    {
        let mut lib = state.lib();
        lib.data.settings.ptt_enabled = enabled;
        lib.data.settings.ptt_hotkey = hotkey.filter(|h| !h.code.is_empty());
        lib.save()?;
    }
    input::set_ptt_held(false);
    input::sync_from_library();
    Ok(())
}

/// Ses efekti çalarken fiziksel mikrofonu geçici olarak susturma ayarını günceller.
#[tauri::command]
pub fn set_mute_mic_on_play(state: State<'_, AppState>, enabled: bool) -> CmdResult<()> {
    state.engine.set_mute_mic_on_play(enabled);
    let mut lib = state.lib();
    lib.data.settings.mute_mic_on_play = enabled;
    lib.save()
}

/// İlk açılış kurulum ekranını tamamlar ve tercihleri kaydeder.
#[tauri::command]
pub fn complete_onboarding(
    state: State<'_, AppState>,
    ptt_enabled: bool,
    ptt_hotkey: Option<HotkeyBinding>,
    mute_mic_on_play: bool,
) -> CmdResult<()> {
    state.engine.set_mute_mic_on_play(mute_mic_on_play);
    {
        let mut lib = state.lib();
        lib.data.settings.ptt_enabled = ptt_enabled;
        lib.data.settings.ptt_hotkey = ptt_hotkey.filter(|h| !h.code.is_empty());
        lib.data.settings.mute_mic_on_play = mute_mic_on_play;
        lib.data.settings.onboarding_done = true;
        lib.save()?;
    }
    input::sync_from_library();
    Ok(())
}

/// `folder_id` yoksa "Tüm Sesler" sırası, varsa o klasörün sırası güncellenir.
#[tauri::command]
pub fn reorder_sounds(
    state: State<'_, AppState>,
    folder_id: Option<String>,
    ids: Vec<String>,
) -> CmdResult<()> {
    let mut lib = state.lib();
    match folder_id {
        None => {
            let current: Vec<String> = lib.data.sounds.iter().map(|s| s.id.clone()).collect();
            let order = apply_order(&current, &ids);
            let mut sounds = std::mem::take(&mut lib.data.sounds);
            let mut sorted = Vec::with_capacity(sounds.len());
            for id in &order {
                if let Some(pos) = sounds.iter().position(|s| &s.id == id) {
                    sorted.push(sounds.swap_remove(pos));
                }
            }
            sorted.extend(sounds);
            lib.data.sounds = sorted;
        }
        Some(fid) => {
            let f = lib.folder_mut(&fid)?;
            f.sounds = apply_order(&f.sounds, &ids);
        }
    }
    lib.save()
}

#[tauri::command]
pub fn create_folder(state: State<'_, AppState>, name: String) -> CmdResult<Folder> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Klasör adı boş olamaz".into());
    }
    let mut lib = state.lib();
    let folder = Folder {
        id: library::new_id("f"),
        name,
        sounds: Vec::new(),
    };
    lib.data.folders.push(folder.clone());
    lib.save()?;
    Ok(folder)
}

#[tauri::command]
pub fn rename_folder(state: State<'_, AppState>, id: String, name: String) -> CmdResult<()> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Klasör adı boş olamaz".into());
    }
    let mut lib = state.lib();
    lib.folder_mut(&id)?.name = name;
    lib.save()
}

/// Klasörü siler; içindeki sesler "Tüm Sesler"de kalır.
#[tauri::command]
pub fn delete_folder(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let mut lib = state.lib();
    lib.data.folders.retain(|f| f.id != id);
    lib.save()
}

#[tauri::command]
pub fn add_to_folder(
    state: State<'_, AppState>,
    folder_id: String,
    ids: Vec<String>,
) -> CmdResult<()> {
    let mut lib = state.lib();
    let valid: Vec<String> = ids.into_iter().filter(|id| lib.sound(id).is_some()).collect();
    let f = lib.folder_mut(&folder_id)?;
    for id in valid {
        if !f.sounds.contains(&id) {
            f.sounds.push(id);
        }
    }
    lib.save()
}

/// Yalnızca klasör bağlantısını kaldırır; ses kütüphanede kalır.
#[tauri::command]
pub fn remove_from_folder(
    state: State<'_, AppState>,
    folder_id: String,
    ids: Vec<String>,
) -> CmdResult<()> {
    let mut lib = state.lib();
    let f = lib.folder_mut(&folder_id)?;
    f.sounds.retain(|id| !ids.contains(id));
    lib.save()
}

#[tauri::command]
pub fn play_sound(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let (path, gain, replay_mode, duration) = {
        let lib = state.lib();
        let s = lib.sound(&id).ok_or("Ses bulunamadı")?;
        (lib.sound_path(s), s.effective_gain(), s.replay_mode, s.duration)
    };
    state
        .engine
        .play_ext(id, &path, gain, replay_mode, duration, None, None)
}

/// Ses kırpma ekranındaki "1 Kerelik Çal" işlemi: dosyayı değiştirmeden yalnızca `[start_sec, end_sec]` aralığını çalar.
#[tauri::command]
pub fn play_sound_range(
    state: State<'_, AppState>,
    id: String,
    start_sec: f32,
    end_sec: f32,
) -> CmdResult<()> {
    if end_sec <= start_sec || start_sec < 0.0 {
        return Err("Geçersiz zaman aralığı".into());
    }
    let (path, gain, duration) = {
        let lib = state.lib();
        let s = lib.sound(&id).ok_or("Ses bulunamadı")?;
        (lib.sound_path(s), s.effective_gain(), s.duration)
    };
    state.engine.play_ext(
        id,
        &path,
        gain,
        ReplayMode::Restart,
        duration,
        Some(start_sec),
        Some(end_sec),
    )
}

/// Ses kırpma ekranı için dalga formu tepe noktalarını ve kesin süreyi döndürür.
#[tauri::command]
pub async fn get_waveform(app: AppHandle, id: String) -> CmdResult<WaveformData> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let path = {
            let lib = state.lib();
            let s = lib.sound(&id).ok_or("Ses bulunamadı")?;
            lib.sound_path(s)
        };
        let wf = trimmer::compute_waveform(&path, 240)?;
        if wf.duration > 0.0 {
            let mut lib = state.lib();
            if let Ok(s) = lib.sound_mut(&id) {
                if (s.duration - wf.duration).abs() > 0.05 {
                    s.duration = wf.duration;
                    let _ = lib.save();
                }
            }
        }
        Ok(wf)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Seçilen `[start_sec, end_sec]` aralığını kalıcı ses dosyası olarak kaydeder.
#[tauri::command]
pub async fn save_trimmed_sound(
    app: AppHandle,
    id: String,
    start_sec: f32,
    end_sec: f32,
) -> CmdResult<Sound> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state.engine.stop_all();
        let (sounds_dir, old_file, normalized) = {
            let lib = state.lib();
            let s = lib.sound(&id).ok_or("Ses bulunamadı")?;
            (lib.sounds_dir.clone(), s.file.clone(), s.normalized)
        };
        let (new_file, new_gain, new_dur) =
            trimmer::trim_to_file(&sounds_dir, &old_file, start_sec, end_sec, normalized)?;
        let mut lib = state.lib();
        let s = lib.sound_mut(&id)?;
        s.file = new_file;
        s.gain = new_gain;
        s.duration = new_dur;
        let updated = s.clone();
        lib.save()?;
        Ok(updated)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Profil yedekleme (.zip dışa aktarma).
#[tauri::command]
pub async fn export_profile_backup(app: AppHandle) -> CmdResult<Option<String>> {
    tauri::async_runtime::spawn_blocking(move || {
        let Some(zip_path) = backup::pick_save_zip_path() else {
            return Ok(None);
        };
        let state = app.state::<AppState>();
        let (data, sounds_dir) = {
            let lib = state.lib();
            (lib.data.clone(), lib.sounds_dir.clone())
        };
        backup::export_zip(&data, &sounds_dir, &zip_path)?;
        Ok(Some(zip_path.display().to_string()))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Profil geri yükleme (.zip içe aktarma).
#[tauri::command]
pub async fn import_profile_backup(app: AppHandle) -> CmdResult<Option<LibraryData>> {
    tauri::async_runtime::spawn_blocking(move || {
        let Some(zip_path) = backup::pick_open_zip_path() else {
            return Ok(None);
        };
        let state = app.state::<AppState>();
        state.engine.stop_all();
        let sounds_dir = state.lib().sounds_dir.clone();
        let restored = backup::import_zip(&sounds_dir, &zip_path)?;
        let out_data = {
            let mut lib = state.lib();
            if let Some(data) = restored {
                lib.data = data;
            }
            lib.reconcile();
            lib.save()?;
            let s = &lib.data.settings;
            state.engine.set_volume(s.volume);
            state.engine.set_local_preview(s.local_preview);
            state.engine.set_mute_mic_on_play(s.mute_mic_on_play);
            state.engine.set_monitor(s.monitor_device.clone());
            state.engine.set_broadcast(s.broadcast_device.clone());
            state.engine.set_mic(s.mic_device.clone());
            lib.data.clone()
        };
        input::sync_from_library();
        Ok(Some(out_data))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn stop_all(state: State<'_, AppState>) {
    state.engine.stop_all();
}

#[tauri::command]
pub fn set_volume(state: State<'_, AppState>, volume: f32, persist: bool) -> CmdResult<()> {
    let v = volume.clamp(0.0, 1.0);
    state.engine.set_volume(v);
    if persist {
        let mut lib = state.lib();
        lib.data.settings.volume = v;
        lib.save()?;
    }
    Ok(())
}

#[tauri::command]
pub fn set_local_preview(state: State<'_, AppState>, enabled: bool) -> CmdResult<()> {
    state.engine.set_local_preview(enabled);
    let mut lib = state.lib();
    lib.data.settings.local_preview = enabled;
    lib.save()
}

#[tauri::command]
pub fn set_normalize_choice(state: State<'_, AppState>, choice: Option<bool>) -> CmdResult<()> {
    let mut lib = state.lib();
    lib.data.settings.normalize_choice = choice;
    lib.save()
}

#[tauri::command]
pub async fn list_devices(app: AppHandle) -> CmdResult<DeviceList> {
    tauri::async_runtime::spawn_blocking(move || app.state::<AppState>().engine.list_devices())
        .await
        .map_err(|e| e.to_string())?
}

/// `kind`: "mic" | "monitor" | "broadcast"
#[tauri::command]
pub fn set_device(
    state: State<'_, AppState>,
    kind: String,
    name: Option<String>,
) -> CmdResult<()> {
    let name = name.filter(|n| !n.is_empty());
    let mut lib = state.lib();
    match kind.as_str() {
        "mic" => {
            lib.data.settings.mic_device = name.clone();
            state.engine.set_mic(name);
        }
        "monitor" => {
            lib.data.settings.monitor_device = name.clone();
            state.engine.set_monitor(name);
        }
        "broadcast" => {
            lib.data.settings.broadcast_device = name.clone();
            state.engine.set_broadcast(name);
        }
        other => return Err(format!("Bilinmeyen cihaz türü: {other}")),
    }
    lib.save()
}

#[tauri::command]
pub fn audio_status(state: State<'_, AppState>) -> AudioStatus {
    state.engine.status()
}

/// APO (`ozan-apo.dll`) SHA-256 bütünlük, `FxProperties` zincir ve mod ("APO Aktif" / "Fallback (Doğrudan Çıkış)") durumunu döndürür.
#[tauri::command]
pub fn get_apo_status(state: State<'_, AppState>) -> ApoHealthReport {
    state.apo.inspect_health()
}

/// Tek tıkla otomatik APO onarımı (Self-Healing):
/// `ozan-apo.dll` bütünlüğünü onarır, COM CLSID ve aktif mikrofonun `FxProperties` APO zincirini
/// (üretici efektlerini koruyarak) yeniden kurar.
#[tauri::command]
pub fn repair_apo(state: State<'_, AppState>) -> ApoHealthReport {
    state.apo.repair()
}

#[tauri::command]
pub fn open_sounds_folder(state: State<'_, AppState>) -> CmdResult<()> {
    let dir = state.lib().sounds_dir.clone();
    std::process::Command::new("explorer.exe")
        .arg(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
