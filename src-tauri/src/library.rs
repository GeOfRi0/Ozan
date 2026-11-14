//! Kütüphane modeli: sesler, klasörler, kısayollar ve ayarlar.
//! `%APPDATA%\Ozan\library.json` dosyasında atomik olarak saklanır.
//! Ses dosyalarının kendisi `%APPDATA%\Ozan\sounds\` altındadır.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SUPPORTED_EXTS: &[&str] = &["opus", "mp3", "wav", "ogg", "flac"];

/// Aynı ses kartı tekrar tetiklendiğinde uygulanacak davranış.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReplayMode {
    /// Mevcut oynatmayı kesip 0. saniyeden yeniden başlatır.
    Restart,
    /// Mevcut oynatma sürerken yeni bir kopyasını üst üste bindirir (maks. 5).
    Overlap,
}

impl Default for ReplayMode {
    fn default() -> Self {
        Self::Restart
    }
}

/// Klavye, standart/ek fare tuşu veya Raw Input / HID (MMO faresi) kısayol tanımı.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyBinding {
    /// Benzersiz tuş kodu (örn. "VK:35", "MOUSE:X1", "MOUSE:X2", "MOUSE:B6", "HID:046d:c08b:9:12").
    pub code: String,
    /// Kullanıcı arayüzünde gösterilen okunabilir etiket (örn. "End", "Mouse 4 (XBUTTON1)").
    pub label: String,
    /// Tuş basımı aktif oyuna/uygulamaya iletilsin mi?
    #[serde(default = "default_true")]
    pub passthrough: bool,
}

impl HotkeyBinding {
    pub fn default_panic() -> Self {
        // VK_END = 0x23 (35)
        Self {
            code: "VK:35".to_string(),
            label: "End".to_string(),
            passthrough: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_user_gain() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sound {
    pub id: String,
    pub name: String,
    /// `sounds` dizinine göre dosya adı.
    pub file: String,
    /// Normalize tepe dengeleme kazancı (-1 dB için).
    pub gain: f32,
    /// Kullanıcının kart üzerinden ayarladığı bireysel ses kazancı (0.0 ..= 2.0, %0 - %200).
    #[serde(default = "default_user_gain")]
    pub user_gain: f32,
    pub normalized: bool,
    pub favorite: bool,
    /// Saniye cinsinden gerçek süre (üst sınır yoktur).
    #[serde(default)]
    pub duration: f32,
    /// Özel renk etiketi (örn. "#7c6cff", boşsa varsayılan).
    #[serde(default)]
    pub color: String,
    /// Aynı tuşa tekrar basıldığında uygulanacak tekrar modu.
    #[serde(default)]
    pub replay_mode: ReplayMode,
    /// Karta atanmış kısayol tuşu ve Passthrough ayarı.
    #[serde(default)]
    pub hotkey: Option<HotkeyBinding>,
}

impl Sound {
    /// Mikserde uygulanacak toplam doğrusal kazanç.
    pub fn effective_gain(&self) -> f32 {
        (self.gain * self.user_gain.clamp(0.0, 2.0)).max(0.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: String,
    pub name: String,
    /// Klasördeki seslerin sıralı kimlikleri (yalnızca bağlantı).
    pub sounds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub volume: f32,
    pub local_preview: bool,
    pub mic_device: Option<String>,
    pub broadcast_device: Option<String>,
    pub monitor_device: Option<String>,
    /// "Seçimimi hatırla" işaretlendiyse kalıcı normalize tercihi.
    pub normalize_choice: Option<bool>,
    /// Acil durdurma (Panic) kısayolu (varsayılan: VK_END / "End").
    pub panic_hotkey: HotkeyBinding,
    /// İlk açılış kurulum sihirbazı tamamlandı mı?
    pub onboarding_done: bool,
    /// Bas-Konuş (Push-to-Talk) aktif mi?
    pub ptt_enabled: bool,
    /// Bas-Konuş tuşu (SendInput ile ses çaldığı sürece basılı tutulur).
    pub ptt_hotkey: Option<HotkeyBinding>,
    /// Ses efekti çalarken fiziksel mikrofon geçici olarak susturulsun mu?
    pub mute_mic_on_play: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.8,
            local_preview: false,
            mic_device: None,
            broadcast_device: None,
            monitor_device: None,
            normalize_choice: None,
            panic_hotkey: HotkeyBinding::default_panic(),
            onboarding_done: false,
            ptt_enabled: false,
            ptt_hotkey: None,
            mute_mic_on_play: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LibraryData {
    pub version: u32,
    /// "Tüm Sesler" sırası.
    pub sounds: Vec<Sound>,
    pub folders: Vec<Folder>,
    pub settings: Settings,
}

impl Default for LibraryData {
    fn default() -> Self {
        Self {
            version: 2,
            sounds: Vec::new(),
            folders: Vec::new(),
            settings: Settings::default(),
        }
    }
}

pub struct Library {
    pub data: LibraryData,
    #[allow(dead_code)]
    pub root: PathBuf,
    pub sounds_dir: PathBuf,
    pub db_path: PathBuf,
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn new_id(prefix: &str) -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
    let c = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{t:x}{c:x}")
}

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// `%APPDATA%\Ozan`
pub fn app_root() -> Result<PathBuf, String> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "APPDATA ortam değişkeni bulunamadı".to_string())?;
    Ok(base.join("Ozan"))
}

impl Library {
    pub fn load() -> Result<Self, String> {
        let root = app_root()?;
        let sounds_dir = root.join("sounds");
        fs::create_dir_all(&sounds_dir).map_err(|e| format!("Dizin oluşturulamadı: {e}"))?;
        let db_path = root.join("library.json");

        let data = match fs::read(&db_path) {
            Ok(bytes) => match serde_json::from_slice::<LibraryData>(&bytes) {
                Ok(d) => d,
                Err(e) => {
                    let bak = root.join(format!("library.broken-{}.json", new_id("")));
                    let _ = fs::rename(&db_path, &bak);
                    log::error!("library.json okunamadı ({e}); yedek: {}", bak.display());
                    LibraryData::default()
                }
            },
            Err(_) => LibraryData::default(),
        };

        let mut lib = Self { data, root, sounds_dir, db_path };
        lib.reconcile();
        lib.save()?;
        Ok(lib)
    }

    /// Diskte olmayan sesleri kaldırır, `sounds` dizininde olup kayıtlı olmayan
    /// desteklenen dosyaları "Tüm Sesler"e ekler ve eksik süreleri tamamlar.
    pub fn reconcile(&mut self) {
        let dir = self.sounds_dir.clone();
        self.data.sounds.retain(|s| dir.join(&s.file).is_file());
        let known: HashSet<String> = self.data.sounds.iter().map(|s| s.file.to_lowercase()).collect();
        if let Ok(rd) = fs::read_dir(&dir) {
            let mut extra: Vec<PathBuf> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && is_supported(p))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| !known.contains(&n.to_lowercase()))
                        .unwrap_or(false)
                })
                .collect();
            extra.sort();
            for p in extra {
                let file = p.file_name().unwrap().to_string_lossy().to_string();
                let duration = crate::audio::probe_duration(&p);
                self.data.sounds.push(Sound {
                    id: new_id("s"),
                    name: display_name(&p),
                    file,
                    gain: 1.0,
                    user_gain: 1.0,
                    normalized: false,
                    favorite: false,
                    duration,
                    color: String::new(),
                    replay_mode: ReplayMode::Restart,
                    hotkey: None,
                });
            }
        }
        for s in &mut self.data.sounds {
            s.user_gain = s.user_gain.clamp(0.0, 2.0);
            if s.duration <= 0.0 {
                s.duration = crate::audio::probe_duration(&dir.join(&s.file));
            }
        }
        let ids: HashSet<String> = self.data.sounds.iter().map(|s| s.id.clone()).collect();
        for f in &mut self.data.folders {
            let mut seen = HashSet::new();
            f.sounds.retain(|id| ids.contains(id) && seen.insert(id.clone()));
        }
        self.data.settings.volume = self.data.settings.volume.clamp(0.0, 1.0);
        if self.data.settings.panic_hotkey.code.is_empty() {
            self.data.settings.panic_hotkey = HotkeyBinding::default_panic();
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(&self.data).map_err(|e| e.to_string())?;
        let tmp = self.db_path.with_extension("json.tmp");
        fs::write(&tmp, json).map_err(|e| format!("Kütüphane yazılamadı: {e}"))?;
        fs::rename(&tmp, &self.db_path).map_err(|e| format!("Kütüphane kaydedilemedi: {e}"))
    }

    pub fn sound(&self, id: &str) -> Option<&Sound> {
        self.data.sounds.iter().find(|s| s.id == id)
    }

    pub fn sound_mut(&mut self, id: &str) -> Result<&mut Sound, String> {
        self.data.sounds.iter_mut().find(|s| s.id == id).ok_or_else(|| "Ses bulunamadı".into())
    }

    pub fn sound_path(&self, s: &Sound) -> PathBuf {
        self.sounds_dir.join(&s.file)
    }

    pub fn folder_mut(&mut self, id: &str) -> Result<&mut Folder, String> {
        self.data.folders.iter_mut().find(|f| f.id == id).ok_or_else(|| "Klasör bulunamadı".into())
    }

    /// Sesleri kütüphaneden ve diskten tamamen siler (tüm klasör bağlantılarıyla birlikte).
    pub fn delete_sounds(&mut self, ids: &[String]) -> Vec<String> {
        let set: HashSet<&String> = ids.iter().collect();
        let mut errors = Vec::new();
        let dir = self.sounds_dir.clone();
        self.data.sounds.retain(|s| {
            if !set.contains(&s.id) {
                return true;
            }
            if let Err(e) = fs::remove_file(dir.join(&s.file)) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    errors.push(format!("{}: {e}", s.name));
                    return true;
                }
            }
            false
        });
        let remaining: HashSet<String> = self.data.sounds.iter().map(|s| s.id.clone()).collect();
        for f in &mut self.data.folders {
            f.sounds.retain(|id| remaining.contains(id));
        }
        errors
    }
}

/// Verilen kimlik sırasını `current` listesine uygular; bilinmeyenleri yok sayar,
/// listede olmayanları sona ekler.
pub fn apply_order(current: &[String], order: &[String]) -> Vec<String> {
    let existing: HashSet<&String> = current.iter().collect();
    let mut seen = HashSet::new();
    let mut out: Vec<String> = order
        .iter()
        .filter(|id| existing.contains(id) && seen.insert((*id).clone()))
        .cloned()
        .collect();
    for id in current {
        if seen.insert(id.clone()) {
            out.push(id.clone())
        }
    }
    out
}

/// Kaynak dosyayı `sounds_dir` içine benzersiz bir adla kopyalar (orijinalden bağımsız).
pub fn copy_into(sounds_dir: &Path, src: &Path) -> Result<PathBuf, String> {
    if !src.is_file() {
        return Err(format!("Dosya bulunamadı: {}", src.display()));
    }
    let stem = sanitize(&src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
    let stem = if stem.is_empty() { "ses".to_string() } else { stem };
    let ext = src.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let mut dest = sounds_dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while dest.exists() {
        dest = sounds_dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    fs::copy(src, &dest).map_err(|e| format!("Kopyalanamadı ({}): {e}", src.display()))?;
    Ok(dest)
}

pub fn display_name(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().replace('_', " ")).unwrap_or_else(|| "Ses".into())
}

pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c })
        .collect::<String>()
        .trim()
        .trim_end_matches('.')
        .chars()
        .take(120)
        .collect()
}
