//! Ozan - hafif Windows soundboard.

pub mod apo;
mod audio;
mod backup;
mod commands;
mod input;
mod library;
mod tray;
mod trimmer;

use std::sync::Mutex;

use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};

use audio::{AudioEngine, DeviceConfig};
use commands::AppState;
use library::Library;

pub const MAIN_WINDOW: &str = "main";

/// Ana pencereyi gösterir; tepsiye küçültülürken yok edildiyse yeniden oluşturur.
/// (Pencere kapatıldığında WebView yok edilir; böylece arka planda RAM tüketimi
/// yalnızca Rust çekirdeğinden ibaret kalır.)
pub fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let builder = WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
        .title("Ozan")
        .inner_size(1160.0, 760.0)
        .min_inner_size(820.0, 540.0)
        .center()
        .theme(Some(tauri::Theme::Dark))
        .background_color(tauri::window::Color(15, 16, 20, 255));
    let built = match app.default_window_icon() {
        Some(icon) => match builder.icon(icon.clone()) {
            Ok(b) => b.build(),
            Err(e) => Err(e),
        },
        None => builder.build(),
    };
    if let Err(e) = built {
        log::error!("Pencere oluşturulamadı: {e}");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main_window(app)))
        .setup(|app| {
            let lib = Library::load()?;
            let apo = apo::ApoManager::init(lib.root.clone());
            let s = &lib.data.settings;
            let engine = AudioEngine::start(
                s.volume,
                s.local_preview,
                s.mute_mic_on_play,
                DeviceConfig {
                    monitor: s.monitor_device.clone(),
                    broadcast: s.broadcast_device.clone(),
                    mic: s.mic_device.clone(),
                },
            )?;
            app.manage(AppState {
                lib: Mutex::new(lib),
                engine,
                apo,
            });
            input::init(app.handle().clone());
            tray::create(app.handle())?;
            if !std::env::args().any(|a| a == "--minimized") {
                show_main_window(app.handle());
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_library,
            commands::import_files,
            commands::delete_sounds,
            commands::rename_sound,
            commands::set_favorite,
            commands::set_sound_gain,
            commands::set_sound_color,
            commands::set_sound_replay_mode,
            commands::set_sound_hotkey,
            commands::start_hotkey_capture,
            commands::cancel_hotkey_capture,
            commands::set_panic_hotkey,
            commands::set_ptt_config,
            commands::set_mute_mic_on_play,
            commands::complete_onboarding,
            commands::reorder_sounds,
            commands::create_folder,
            commands::rename_folder,
            commands::delete_folder,
            commands::add_to_folder,
            commands::remove_from_folder,
            commands::play_sound,
            commands::play_sound_range,
            commands::get_waveform,
            commands::save_trimmed_sound,
            commands::export_profile_backup,
            commands::import_profile_backup,
            commands::stop_all,
            commands::set_volume,
            commands::set_local_preview,
            commands::set_normalize_choice,
            commands::list_devices,
            commands::set_device,
            commands::audio_status,
            commands::get_apo_status,
            commands::repair_apo,
            commands::open_sounds_folder,
        ])
        .build(tauri::generate_context!());

    let app = match app {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Ozan başlatılamadı: {e}");
            std::process::exit(1);
        }
    };

    app.run(|app, event| match event {
        // Son pencere kapandığında uygulama kapanmaz; tepside çalışmaya devam eder.
        RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        RunEvent::Exit => {
            input::set_ptt_held(false);
            if let Some(state) = app.try_state::<AppState>() {
                state.engine.shutdown();
            }
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::mixer::{MixCmd, Sink, MIX_RATE};
    use crate::audio::source::SampleSource;
    use crate::audio::Shared;
    use crate::library::{HotkeyBinding, LibraryData, ReplayMode, Settings};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    struct ConstSource {
        frames_left: usize,
        buf: Vec<f32>,
    }

    impl ConstSource {
        fn new(val: f32, frames: usize) -> Self {
            Self {
                frames_left: frames,
                buf: vec![val; 480], // 240 stereo frames
            }
        }
    }

    impl SampleSource for ConstSource {
        fn sample_rate(&self) -> u32 {
            MIX_RATE
        }
        fn channels(&self) -> usize {
            2
        }
        fn duration(&self) -> Option<f32> {
            Some(1.0)
        }
        fn next_block(&mut self) -> Result<Option<(&[f32], u32, usize)>, String> {
            if self.frames_left == 0 {
                return Ok(None);
            }
            let take = self.frames_left.min(240);
            self.frames_left -= take;
            Ok(Some((&self.buf[..take * 2], MIX_RATE, 2)))
        }
    }

    #[test]
    fn test_default_panic_hotkey_is_end() {
        let s = Settings::default();
        assert_eq!(s.panic_hotkey.code, "VK:35");
        assert_eq!(s.panic_hotkey.label, "End");
        assert!(!s.onboarding_done);
        let b = HotkeyBinding::default_panic();
        assert_eq!(b.code, "VK:35");
    }

    #[test]
    fn test_fifo_5_voice_limit_and_no_ducking() {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::new(1.0, true, false));
        let shared_c = shared.clone();
        let handle = std::thread::spawn(move || {
            crate::audio::mixer::run(rx, shared_c);
        });

        let (prod, mut cons) = rtrb::RingBuffer::<f32>::new(48_000);
        tx.send(MixCmd::SetMonitor(Some(Sink::new(prod, MIX_RATE)))).unwrap();

        // 6 farklı ses gönder (her biri 0.1 değerinde, uzun süreli).
        // İlk 5 ses + 6. ses geldiğinde en eski 1. ses FIFO hard-cut ile kesilmeli,
        // toplam 5 ses kalmalı ve ducking olmadan toplamları 5 * 0.1 = 0.5 olmalı.
        for i in 1..=6 {
            tx.send(MixCmd::Play {
                id: format!("s{i}"),
                source: Box::new(ConstSource::new(0.1, 48_000)),
                gain: 1.0,
                replay_mode: ReplayMode::Restart,
                total_duration: 1.0,
                start_sec: None,
                end_sec: None,
            })
            .unwrap();
        }

        std::thread::sleep(Duration::from_millis(50));
        let prog = shared.get_progress();
        assert_eq!(prog.len(), 5, "En fazla 5 eşzamanlı ses olmalı");
        assert!(prog.iter().all(|p| p.id != "s1"), "En eski ses (s1) FIFO ile kesilmeli");
        assert!(prog.iter().any(|p| p.id == "s6"), "Yeni ses (s6) aktif olmalı");

        let avail = cons.slots();
        assert!(avail > 0, "Çıkış tamponuna örnek yazılmış olmalı");
        if let Ok(chunk) = cons.read_chunk(avail) {
            let first_sample = chunk.into_iter().next().unwrap_or(0.0);
            assert!(
                (first_sample - 0.5).abs() < 1e-4,
                "Ducking uygulanmamalı: 5 * 0.1 = 0.5 beklenirken {first_sample} bulundu"
            );
        }

        tx.send(MixCmd::Shutdown).unwrap();
        let _ = handle.join();
    }

    #[test]
    fn test_trim_and_backup_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("ozan_test_{}", library::new_id("t")));
        let sounds_dir = tmp.join("sounds");
        std::fs::create_dir_all(&sounds_dir).unwrap();

        // 1 saniyelik 48kHz stereo 16-bit PCM test WAV oluştur
        let wav_path = sounds_dir.join("test.wav");
        {
            let frames = 48_000u32;
            let data_bytes = frames * 4;
            let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
            bytes.extend_from_slice(b"RIFF");
            bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
            bytes.extend_from_slice(b"WAVEfmt ");
            bytes.extend_from_slice(&16u32.to_le_bytes());
            bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
            bytes.extend_from_slice(&2u16.to_le_bytes()); // stereo
            bytes.extend_from_slice(&48_000u32.to_le_bytes());
            bytes.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
            bytes.extend_from_slice(&4u16.to_le_bytes());
            bytes.extend_from_slice(&16u16.to_le_bytes());
            bytes.extend_from_slice(b"data");
            bytes.extend_from_slice(&data_bytes.to_le_bytes());
            for _ in 0..frames {
                bytes.extend_from_slice(&12000i16.to_le_bytes());
                bytes.extend_from_slice(&(-12000i16).to_le_bytes());
            }
            std::fs::write(&wav_path, &bytes).unwrap();
        }

        // Waveform hesapla
        let wf = trimmer::compute_waveform(&wav_path, 50).unwrap();
        assert_eq!(wf.peaks.len(), 50);
        assert!((wf.duration - 1.0).abs() < 0.05);

        // 0.2s - 0.7s (0.5s) olarak kırp
        let (new_file, _gain, new_dur) =
            trimmer::trim_to_file(&sounds_dir, "test.wav", 0.2, 0.7, false).unwrap();
        assert_eq!(new_file, "test.wav");
        assert!((new_dur - 0.5).abs() < 0.05, "Kırpılmış süre ~0.5s olmalı: {new_dur}");

        // Yedekleme ve geri yükleme testi
        let mut lib_data = LibraryData::default();
        lib_data.settings.onboarding_done = true;
        lib_data.sounds.push(crate::library::Sound {
            id: "s_test".into(),
            name: "Test".into(),
            file: "test.wav".into(),
            favorite: false,
            gain: 1.0,
            user_gain: 1.5,
            color: "#6366f1".into(),
            replay_mode: ReplayMode::Overlap,
            hotkey: None,
            normalized: false,
            duration: new_dur,
        });
        let zip_path = tmp.join("backup.zip");
        let exported_count = backup::export_zip(&lib_data, &sounds_dir, &zip_path).unwrap();
        assert_eq!(exported_count, 1);

        let restore_sounds = tmp.join("restored_sounds");
        let restored = backup::import_zip(&restore_sounds, &zip_path).unwrap().unwrap();
        assert!(restored.settings.onboarding_done);
        assert_eq!(restored.sounds.len(), 1);
        assert_eq!(restored.sounds[0].user_gain, 1.5);
        assert!(restore_sounds.join("test.wav").is_file());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_shared_memory_spsc_and_apo_process_rt() {
        use ozan_apo::{
            DllGetClassObject, IAudioProcessingObjectRTVtbl, IClassFactoryVtbl,
            APO_CONNECTION_PROPERTY, BUFFER_VALID, IID_IAUDIOPROCESSINGOBJECT_RT,
            IID_ICLASSFACTORY,
        };
        use ozan_shm::{SharedMemoryMapping, FLAG_MUTE_PHYSICAL_MIC, OZAN_APO_CLSID};
        use std::ffi::c_void;
        use std::ptr::null_mut;
        use std::sync::atomic::Ordering;

        let shm = SharedMemoryMapping::create_or_open().expect("Paylaşımlı bellek açılmalı");
        let block = shm.block();
        block.clear();
        block.flags.store(0, Ordering::Release);
        block.apo_sample_rate.store(48_000, Ordering::Release);
        block.apo_channels.store(2, Ordering::Release);
        block.inject_gain_bits.store(1.0f32.to_bits(), Ordering::Release);

        // 8 stereo kare (16 f32 örnek) paylaşımlı belleğe yaz (her örnek 0.25)
        let injected = [0.25f32; 16];
        assert_eq!(block.push_samples(&injected), 16);

        unsafe {
            let mut cf_ptr: *mut c_void = null_mut();
            let hr = DllGetClassObject(&OZAN_APO_CLSID, &IID_ICLASSFACTORY, &mut cf_ptr);
            assert_eq!(hr, 0, "DllGetClassObject başarılı olmalı");
            assert!(!cf_ptr.is_null());

            let cf_vtbl = *(cf_ptr as *const *const IClassFactoryVtbl);
            let mut rt_ptr: *mut c_void = null_mut();
            let hr2 = ((*cf_vtbl).CreateInstance)(
                cf_ptr,
                null_mut(),
                &IID_IAUDIOPROCESSINGOBJECT_RT,
                &mut rt_ptr,
            );
            assert_eq!(hr2, 0, "IAudioProcessingObjectRT örneği oluşturulmalı");
            assert!(!rt_ptr.is_null());

            // Fiziksel mikrofon tamponu (0.10 sinyali) + Ozan enjeksiyonu (0.25) = 0.35 olmalı
            let mut mic_buf = [0.10f32; 16];
            let mut conn = APO_CONNECTION_PROPERTY {
                pBuffer: mic_buf.as_mut_ptr() as usize,
                u32ValidFrameCount: 8,
                u32BufferFlags: BUFFER_VALID,
                u32Signature: 0,
            };
            let in_ptr: *const APO_CONNECTION_PROPERTY = &conn;
            let mut out_ptr: *mut APO_CONNECTION_PROPERTY = &mut conn;

            let rt_vtbl = *(rt_ptr as *const *const IAudioProcessingObjectRTVtbl);
            ((*rt_vtbl).APOProcess)(rt_ptr, 1, &in_ptr, 1, &mut out_ptr);

            for &s in &mic_buf {
                assert!(
                    (s - 0.35).abs() < 1e-4,
                    "Mikrofon (0.10) + Enjekte (0.25) = 0.35 beklenirken {s} bulundu"
                );
            }

            // Şimdi FLAG_MUTE_PHYSICAL_MIC aktifken test et:
            // Fiziksel mikrofon (0.10) susturulmalı, yalnızca enjekte edilen (0.40) çıkmalı
            block.flags.store(FLAG_MUTE_PHYSICAL_MIC, Ordering::Release);
            block.push_samples(&[0.40f32; 16]);
            let mut mic_buf2 = [0.10f32; 16];
            let mut conn2 = APO_CONNECTION_PROPERTY {
                pBuffer: mic_buf2.as_mut_ptr() as usize,
                u32ValidFrameCount: 8,
                u32BufferFlags: BUFFER_VALID,
                u32Signature: 0,
            };
            let in_ptr2: *const APO_CONNECTION_PROPERTY = &conn2;
            let mut out_ptr2: *mut APO_CONNECTION_PROPERTY = &mut conn2;
            ((*rt_vtbl).APOProcess)(rt_ptr, 1, &in_ptr2, 1, &mut out_ptr2);
            for &s in &mic_buf2 {
                assert!(
                    (s - 0.40).abs() < 1e-4,
                    "Fiziksel mikrofon susturulduğunda yalnızca 0.40 çıkmalı, {s} bulundu"
                );
            }
            block.flags.store(0, Ordering::Release);

            ((*rt_vtbl).Release)(rt_ptr);
            ((*cf_vtbl).Release)(cf_ptr);
        }
    }

    #[test]
    fn test_apo_sha256_and_self_healing_manager() {
        let tmp = std::env::temp_dir().join(format!("ozan_apo_test_{}", library::new_id("a")));
        let _ = std::fs::create_dir_all(&tmp);
        let mgr = apo::ApoManager::init(tmp.clone());
        let report = mgr.inspect_health();
        assert!(report.shm_connected, "Paylaşımlı bellek bağlı olmalı");
        assert!(!report.mode_label.is_empty());
        let repaired = mgr.repair();
        assert!(repaired.shm_connected);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_opus_decoder_direct() {
        // Saf Rust opus-pure çözücüsünün 48kHz stereo SILK/CELT sessizlik paketini çözdüğünü doğrula
        let mut dec = opus_pure::OpusDecoder::new(48_000, 2).expect("OpusDecoder oluşturulmalı");
        // TOC baytı 0xFC (48kHz CELT Tam Bant, 20ms çerçeve, stereo) + sessizlik çerçevesi
        let packet = [0xFCu8, 0xFF, 0xFE];
        let mut out = vec![0.0f32; 960 * 2];
        let decoded_frames = dec
            .decode(&packet, 960, &mut out)
            .expect("Opus paketi çözülmeli");
        assert_eq!(decoded_frames, 960, "20ms @ 48kHz = 960 stereo kare olmalı");
    }
}

