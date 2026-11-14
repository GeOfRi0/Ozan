//! Ses motoru.
//!
//! Mimari:
//! * **Kontrol thread'i** (`ozan-audio-ctl`): cpal akışlarına (kulaklık,
//!   yayın/sanal kablo, mikrofon) sahiptir; cihaz değişikliklerini, hata
//!   kurtarmayı ve boşta akışların duraklatılmasını yönetir.
//! * **Decoder/mikser thread'i** (`ozan-decoder`, bkz. [`mixer`]): dosyaları
//!   diskten parça parça çözer, kilitsiz halka tamponlara yazar.
//! * **Ses kartı callback'leri**: tamponlardan kilitsiz okur; yayın çıkışı
//!   ayrıca mikrofon sesini (ayrı bir kilitsiz tampondan) karıştırır.

pub mod mixer;
pub mod opus;
pub mod resample;
pub mod source;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use ozan_shm::{OzanSharedBlock, SharedMemoryMapping};
use rtrb::{Consumer, RingBuffer};
use serde::Serialize;

use crate::apo::mmnotify::{query_default_capture_endpoint, spawn_endpoint_watcher, EndpointEvent};
use crate::library::ReplayMode;
use mixer::{MixCmd, Sink, VoiceProgress};

/// Mikrofon durumu.
pub const MIC_OFF: u8 = 0;
pub const MIC_ACTIVE: u8 = 1;
pub const MIC_WAITING: u8 = 2; // mikrofon seçili ama yayın çıkışı yok
pub const MIC_ERROR: u8 = 3;

/// Ayarlarda "sistem varsayılanı" anlamına gelen cihaz adı.
pub const DEFAULT_DEVICE: &str = "__default__";

/// Çıkış tamponu uzunluğu (gecikme üst sınırı).
const OUT_BUFFER_MS: u32 = 80;
const MIC_BUFFER_MS: u32 = 200;
/// Mikrofon gecikmesi bu değeri aşarsa fazlası atılır.
const MIC_MAX_LAG_MS: u32 = 60;
/// Bu kadar süre ses çalınmazsa kulaklık akışı duraklatılır.
const IDLE_PAUSE: Duration = Duration::from_secs(4);

pub struct Shared {
    pub volume: AtomicU32,
    pub local_preview: AtomicBool,
    pub mute_mic_on_play: AtomicBool,
    pub active_voices: AtomicUsize,
    pub mic_state: AtomicU8,
    pub mic_level: AtomicU32,
    pub stream_error: AtomicBool,
    last_activity: AtomicU64,
    epoch: Instant,
    progress: Mutex<Vec<VoiceProgress>>,
    pub shm: Option<SharedMemoryMapping>,
}

impl Shared {
    pub fn new(volume: f32, local_preview: bool, mute_mic_on_play: bool) -> Self {
        let shm = SharedMemoryMapping::create_or_open().ok();
        if let Some(m) = &shm {
            m.block()
                .inject_gain_bits
                .store(volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
        }
        Self {
            volume: AtomicU32::new(volume.clamp(0.0, 1.0).to_bits()),
            local_preview: AtomicBool::new(local_preview),
            mute_mic_on_play: AtomicBool::new(mute_mic_on_play),
            active_voices: AtomicUsize::new(0),
            mic_state: AtomicU8::new(MIC_OFF),
            mic_level: AtomicU32::new(0f32.to_bits()),
            stream_error: AtomicBool::new(false),
            last_activity: AtomicU64::new(0),
            epoch: Instant::now(),
            progress: Mutex::new(Vec::new()),
            shm,
        }
    }

    #[inline(always)]
    pub fn shm_block(&self) -> Option<&OzanSharedBlock> {
        self.shm.as_ref().map(|m| m.block())
    }

    pub fn touch(&self) {
        self.last_activity.store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let now = self.epoch.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_activity.load(Ordering::Relaxed)))
    }

    pub fn update_progress(&self, list: Vec<VoiceProgress>) {
        if let Ok(mut guard) = self.progress.lock() {
            *guard = list;
        }
    }

    pub fn get_progress(&self) -> Vec<VoiceProgress> {
        self.progress.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeviceList {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStatus {
    pub mic: &'static str,
    pub mic_level: f32,
    pub playing: usize,
    pub voices: Vec<VoiceProgress>,
    pub apo_sample_rate: u32,
    pub apo_channels: u32,
    pub apo_heartbeat: u64,
}

enum CtlCmd {
    Play {
        id: String,
        source: Box<dyn source::SampleSource>,
        gain: f32,
        replay_mode: ReplayMode,
        total_duration: f32,
        start_sec: Option<f32>,
        end_sec: Option<f32>,
    },
    SetVoiceGain {
        id: String,
        gain: f32,
    },
    StopAll,
    SetMonitor(Option<String>),
    SetBroadcast(Option<String>),
    SetMic(Option<String>),
    EndpointChanged(EndpointEvent),
    ListDevices(Sender<DeviceList>),
    Shutdown,
}

pub struct DeviceConfig {
    pub monitor: Option<String>,
    pub broadcast: Option<String>,
    pub mic: Option<String>,
}

#[derive(Clone)]
pub struct AudioEngine {
    tx: Sender<CtlCmd>,
    pub shared: Arc<Shared>,
}

impl AudioEngine {
    pub fn start(
        volume: f32,
        local_preview: bool,
        mute_mic_on_play: bool,
        devices: DeviceConfig,
    ) -> Result<Self, String> {
        let shared = Arc::new(Shared::new(volume, local_preview, mute_mic_on_play));
        let (mix_tx, mix_rx) = mpsc::channel::<MixCmd>();
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("ozan-decoder".into())
            .spawn(move || mixer::run(mix_rx, sh))
            .map_err(|e| format!("Decoder thread başlatılamadı: {e}"))?;

        let (tx, rx) = mpsc::channel::<CtlCmd>();
        // IMMNotificationClient ile USB / Bluetooth mikrofon takma-çıkarma olaylarını dinle
        let (ep_tx, ep_rx) = mpsc::channel::<EndpointEvent>();
        spawn_endpoint_watcher(ep_tx);
        let tx_hotplug = tx.clone();
        let _ = std::thread::Builder::new()
            .name("ozan-hotplug-bridge".into())
            .spawn(move || {
                while let Ok(ev) = ep_rx.recv() {
                    if tx_hotplug.send(CtlCmd::EndpointChanged(ev)).is_err() {
                        break;
                    }
                }
            });

        let sh = shared.clone();
        std::thread::Builder::new()
            .name("ozan-audio-ctl".into())
            .spawn(move || {
                let mut ctl = Controller::new(sh, mix_tx, devices);
                ctl.apply_broadcast();
                loop {
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(CtlCmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                        Ok(cmd) => ctl.handle(cmd),
                        Err(RecvTimeoutError::Timeout) => ctl.tick(),
                    }
                }
                let _ = ctl.mix_tx.send(MixCmd::Shutdown);
            })
            .map_err(|e| format!("Ses kontrol thread'i başlatılamadı: {e}"))?;

        Ok(Self { tx, shared })
    }

    pub fn play_ext(
        &self,
        id: String,
        path: &Path,
        gain: f32,
        replay_mode: ReplayMode,
        total_duration: f32,
        start_sec: Option<f32>,
        end_sec: Option<f32>,
    ) -> Result<(), String> {
        let source = source::open_source(path)?;
        let dur = if total_duration > 0.0 {
            total_duration
        } else {
            source.duration().unwrap_or(0.0)
        };
        self.tx
            .send(CtlCmd::Play {
                id,
                source,
                gain,
                replay_mode,
                total_duration: dur,
                start_sec,
                end_sec,
            })
            .map_err(|_| "Ses motoru çalışmıyor".to_string())
    }

    pub fn set_voice_gain(&self, id: String, gain: f32) {
        let _ = self.tx.send(CtlCmd::SetVoiceGain { id, gain });
    }

    pub fn stop_all(&self) {
        let _ = self.tx.send(CtlCmd::StopAll);
    }

    pub fn set_volume(&self, v: f32) {
        let clamped = v.clamp(0.0, 1.0);
        self.shared.volume.store(clamped.to_bits(), Ordering::Relaxed);
        if let Some(shm) = self.shared.shm_block() {
            shm.inject_gain_bits.store(clamped.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn set_local_preview(&self, on: bool) {
        self.shared.local_preview.store(on, Ordering::Relaxed);
    }

    pub fn set_mute_mic_on_play(&self, on: bool) {
        self.shared.mute_mic_on_play.store(on, Ordering::Relaxed);
    }

    pub fn set_monitor(&self, name: Option<String>) {
        let _ = self.tx.send(CtlCmd::SetMonitor(name));
    }

    pub fn set_broadcast(&self, name: Option<String>) {
        let _ = self.tx.send(CtlCmd::SetBroadcast(name));
    }

    pub fn set_mic(&self, name: Option<String>) {
        let _ = self.tx.send(CtlCmd::SetMic(name));
    }

    pub fn list_devices(&self) -> Result<DeviceList, String> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(CtlCmd::ListDevices(tx)).map_err(|_| "Ses motoru çalışmıyor".to_string())?;
        rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "Cihaz listesi alınamadı".to_string())
    }

    pub fn status(&self) -> AudioStatus {
        let playing = self.shared.active_voices.load(Ordering::Relaxed);
        let temp_muted = playing > 0 && self.shared.mute_mic_on_play.load(Ordering::Relaxed);
        let mic = match self.shared.mic_state.load(Ordering::Relaxed) {
            MIC_ACTIVE if temp_muted => "muted",
            MIC_ACTIVE => "active",
            MIC_WAITING => "waiting",
            MIC_ERROR => "error",
            _ => "off",
        };
        let mic_level = if temp_muted {
            0.0
        } else {
            f32::from_bits(self.shared.mic_level.load(Ordering::Relaxed))
        };
        let (apo_sample_rate, apo_channels, apo_heartbeat) = match self.shared.shm_block() {
            Some(b) => (
                b.apo_sample_rate.load(Ordering::Relaxed),
                b.apo_channels.load(Ordering::Relaxed),
                b.apo_heartbeat.load(Ordering::Relaxed),
            ),
            None => (48_000, 2, 0),
        };
        AudioStatus {
            mic,
            mic_level,
            playing,
            voices: self.shared.get_progress(),
            apo_sample_rate,
            apo_channels,
            apo_heartbeat,
        }
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(CtlCmd::Shutdown);
    }
}

/// Dosyanın tepe değerini ve süresini akış halinde (parça parça) hesaplar.
pub fn analyze(path: &Path) -> Result<(f32, f32), String> {
    let mut src = source::open_source(path)?;
    let mut peak = 0.0f32;
    let mut samples: u64 = 0;
    let mut rate = src.sample_rate();
    let mut ch = src.channels();
    while let Some((block, b_rate, b_ch)) = src.next_block()? {
        for &s in block {
            let a = s.abs();
            if a > peak && a.is_finite() {
                peak = a;
            }
        }
        samples += block.len() as u64;
        rate = b_rate;
        ch = b_ch;
    }
    let duration = samples as f32 / ch.max(1) as f32 / rate.max(1) as f32;
    Ok((peak, duration))
}

/// Süreyi metadata'dan ucuzca almaya çalışır; olmazsa dosyayı tarar.
pub fn probe_duration(path: &Path) -> f32 {
    match source::open_source(path) {
        Ok(src) => match src.duration() {
            Some(d) if d > 0.0 => d,
            _ => analyze(path).map(|(_, d)| d).unwrap_or(0.0),
        },
        Err(_) => 0.0,
    }
}

// --------------------------------------------------------------------------
// Kontrol thread'i
// --------------------------------------------------------------------------

struct OutStream {
    stream: cpal::Stream,
    playing: bool,
}

impl OutStream {
    fn resume(&mut self) {
        if !self.playing {
            if let Err(e) = self.stream.play() {
                log::warn!("Akış başlatılamadı: {e}");
            }
            self.playing = true;
        }
    }
    fn pause(&mut self) {
        if self.playing {
            let _ = self.stream.pause();
            self.playing = false;
        }
    }
}

struct MicIn {
    cons: Consumer<f32>,
    rate: u32,
}

struct Controller {
    host: cpal::Host,
    shared: Arc<Shared>,
    mix_tx: Sender<MixCmd>,
    monitor_name: Option<String>,
    broadcast_name: Option<String>,
    mic_name: Option<String>,
    monitor: Option<OutStream>,
    broadcast: Option<OutStream>,
    mic: Option<cpal::Stream>,
}

impl Controller {
    fn new(shared: Arc<Shared>, mix_tx: Sender<MixCmd>, d: DeviceConfig) -> Self {
        Self {
            host: cpal::default_host(),
            shared,
            mix_tx,
            monitor_name: d.monitor,
            broadcast_name: d.broadcast,
            mic_name: d.mic,
            monitor: None,
            broadcast: None,
            mic: None,
        }
    }

    fn handle(&mut self, cmd: CtlCmd) {
        match cmd {
            CtlCmd::Play {
                id,
                source,
                gain,
                replay_mode,
                total_duration,
                start_sec,
                end_sec,
            } => {
                self.ensure_monitor();
                if let Some(b) = self.broadcast.as_mut() {
                    b.resume();
                }
                self.shared.touch();
                let _ = self.mix_tx.send(MixCmd::Play {
                    id,
                    source,
                    gain,
                    replay_mode,
                    total_duration,
                    start_sec,
                    end_sec,
                });
            }
            CtlCmd::SetVoiceGain { id, gain } => {
                let _ = self.mix_tx.send(MixCmd::SetVoiceGain { id, gain });
            }
            CtlCmd::StopAll => {
                let _ = self.mix_tx.send(MixCmd::StopAll);
            }
            CtlCmd::SetMonitor(name) => {
                self.monitor_name = name;
                self.drop_monitor();
            }
            CtlCmd::SetBroadcast(name) => {
                self.broadcast_name = name;
                self.apply_broadcast();
            }
            CtlCmd::SetMic(name) => {
                self.mic_name = name;
                self.apply_broadcast();
            }
            CtlCmd::EndpointChanged(_ev) => {
                // USB / Bluetooth mikrofon takıldı veya çıkarıldı:
                // Host listesini tazele ve aktif mikrofona otomatik bağlan.
                self.host = cpal::default_host();
                self.apply_broadcast();
            }
            CtlCmd::ListDevices(reply) => {
                let _ = reply.send(self.list_devices());
            }
            CtlCmd::Shutdown => {}
        }
    }

    /// Periyodik bakım: hata kurtarma ve boşta akışları duraklatma.
    fn tick(&mut self) {
        if self.shared.stream_error.swap(false, Ordering::Relaxed) {
            log::warn!("Ses cihazı hatası; akışlar yeniden kuruluyor");
            self.drop_monitor();
            self.apply_broadcast();
        }
        let idle = self.shared.active_voices.load(Ordering::Relaxed) == 0 && self.shared.idle_for() > IDLE_PAUSE;
        if idle {
            if let Some(m) = self.monitor.as_mut() {
                m.pause();
            }
            if self.mic.is_none() {
                if let Some(b) = self.broadcast.as_mut() {
                    b.pause();
                }
            }
        }
        let lvl = f32::from_bits(self.shared.mic_level.load(Ordering::Relaxed));
        if lvl > 0.0 && self.mic.is_none() {
            self.shared.mic_level.store(0f32.to_bits(), Ordering::Relaxed);
        }
    }

    fn list_devices(&self) -> DeviceList {
        let inputs = self
            .host
            .input_devices()
            .map(|d| d.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default();
        let outputs = self
            .host
            .output_devices()
            .map(|d| d.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default();
        DeviceList {
            inputs,
            outputs,
            default_input: self.host.default_input_device().and_then(|d| d.name().ok()),
            default_output: self.host.default_output_device().and_then(|d| d.name().ok()),
        }
    }

    fn find_output(&self, name: &Option<String>) -> Option<cpal::Device> {
        match name.as_deref() {
            None | Some(DEFAULT_DEVICE) => self.host.default_output_device(),
            Some(n) => self
                .host
                .output_devices()
                .ok()?
                .find(|d| d.name().map(|x| x == n).unwrap_or(false))
                .or_else(|| self.host.default_output_device()),
        }
    }

    fn find_input(&self, name: &str) -> Option<cpal::Device> {
        if name == DEFAULT_DEVICE {
            return self.host.default_input_device();
        }
        self.host.input_devices().ok()?.find(|d| d.name().map(|x| x == name).unwrap_or(false))
    }

    fn drop_monitor(&mut self) {
        if self.monitor.take().is_some() {
            let _ = self.mix_tx.send(MixCmd::SetMonitor(None));
        }
    }

    fn ensure_monitor(&mut self) {
        if let Some(m) = self.monitor.as_mut() {
            m.resume();
            return;
        }
        let Some(dev) = self.find_output(&self.monitor_name) else {
            log::warn!("Kulaklık çıkış cihazı bulunamadı");
            return;
        };
        match open_output(&dev, None, self.shared.clone()) {
            Ok((stream, prod, rate)) => {
                let _ = self.mix_tx.send(MixCmd::SetMonitor(Some(Sink::new(prod, rate))));
                let mut s = OutStream { stream, playing: false };
                s.resume();
                self.monitor = Some(s);
            }
            Err(e) => log::error!("Kulaklık akışı açılamadı: {e}"),
        }
    }

    /// Fiziksel mikrofon yakalama/APO biçimini ve (varsa) Fallback doğrudan çıkış akışını kurar.
    fn apply_broadcast(&mut self) {
        self.mic = None;
        if self.broadcast.take().is_some() {
            let _ = self.mix_tx.send(MixCmd::SetBroadcast(None));
        }
        self.shared.mic_level.store(0f32.to_bits(), Ordering::Relaxed);

        // Aktif fiziksel mikrofonun örnekleme hızı ve kanal sayısını algıla ve paylaşımlı belleğe bildir
        let target_mic_name = self.mic_name.as_deref().unwrap_or(DEFAULT_DEVICE);
        if let Some(mdev) = self.find_input(target_mic_name) {
            if let Ok(cfg) = mdev.default_input_config() {
                if let Some(shm) = self.shared.shm_block() {
                    shm.apo_sample_rate
                        .store(cfg.sample_rate().0.max(8_000), Ordering::Release);
                    shm.apo_channels
                        .store((cfg.channels() as u32).clamp(1, 16), Ordering::Release);
                }
            }
        } else if let Some(ep) = query_default_capture_endpoint() {
            if let Some(shm) = self.shared.shm_block() {
                shm.apo_sample_rate.store(ep.sample_rate, Ordering::Release);
                shm.apo_channels.store(ep.channels, Ordering::Release);
            }
        }

        let Some(bname) = self.broadcast_name.clone() else {
            // Harici sanal ses sürücüsü seçili değilken bile fiziksel mikrofon ve APO
            // paylaşımlı bellek enjeksiyonu doğrudan aktif çalışır!
            if let Some(mname) = self.mic_name.clone() {
                if let Some(mdev) = self.find_input(&mname) {
                    match open_input(&mdev, self.shared.clone()) {
                        Ok((stream, _cons, _rate)) => {
                            if stream.play().is_ok() {
                                self.mic = Some(stream);
                                self.shared.mic_state.store(MIC_ACTIVE, Ordering::Relaxed);
                                return;
                            }
                        }
                        Err(e) => log::warn!("Mikrofon seviye izleyicisi açılamadı: {e}"),
                    }
                    self.shared.mic_state.store(MIC_ACTIVE, Ordering::Relaxed);
                } else {
                    self.shared.mic_state.store(MIC_ERROR, Ordering::Relaxed);
                }
            } else {
                self.shared.mic_state.store(MIC_OFF, Ordering::Relaxed);
            }
            return;
        };
        let Some(bdev) = self.find_output(&Some(bname)) else {
            self.shared
                .mic_state
                .store(if self.mic_name.is_some() { MIC_ERROR } else { MIC_OFF }, Ordering::Relaxed);
            return;
        };

        let mut mic_in = None;
        let mut mic_state = MIC_OFF;
        if let Some(mname) = self.mic_name.clone() {
            mic_state = MIC_ERROR;
            if let Some(mdev) = self.find_input(&mname) {
                match open_input(&mdev, self.shared.clone()) {
                    Ok((stream, cons, rate)) => {
                        if stream.play().is_ok() {
                            self.mic = Some(stream);
                            mic_in = Some(MicIn { cons, rate });
                            mic_state = MIC_ACTIVE;
                        }
                    }
                    Err(e) => log::error!("Mikrofon açılamadı: {e}"),
                }
            }
        }

        match open_output(&bdev, mic_in, self.shared.clone()) {
            Ok((stream, prod, rate)) => {
                let _ = self.mix_tx.send(MixCmd::SetBroadcast(Some(Sink::new(prod, rate))));
                let mut s = OutStream { stream, playing: false };
                if self.mic.is_some() {
                    s.resume();
                }
                self.broadcast = Some(s);
            }
            Err(e) => {
                log::error!("Yayın akışı açılamadı: {e}");
                self.mic = None;
                if self.mic_name.is_some() {
                    mic_state = MIC_ERROR;
                }
            }
        }
        self.shared.mic_state.store(mic_state, Ordering::Relaxed);
    }
}

// --------------------------------------------------------------------------
// cpal akış kurulumları
// --------------------------------------------------------------------------

type OutResult = Result<(cpal::Stream, rtrb::Producer<f32>, u32), String>;

fn open_output(dev: &cpal::Device, mic: Option<MicIn>, shared: Arc<Shared>) -> OutResult {
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let fmt = cfg.sample_format();
    let config: StreamConfig = cfg.into();
    let rate = config.sample_rate.0;
    let cap = ((rate * OUT_BUFFER_MS / 1000) as usize * 2).max(2048) & !1;
    let (prod, cons) = RingBuffer::<f32>::new(cap);
    let stream = match fmt {
        SampleFormat::F32 => build_output::<f32>(dev, &config, cons, mic, shared),
        SampleFormat::I16 => build_output::<i16>(dev, &config, cons, mic, shared),
        SampleFormat::U16 => build_output::<u16>(dev, &config, cons, mic, shared),
        SampleFormat::I32 => build_output::<i32>(dev, &config, cons, mic, shared),
        other => return Err(format!("Desteklenmeyen örnek biçimi: {other:?}")),
    }?;
    Ok((stream, prod, rate))
}

struct MicReader {
    cons: Consumer<f32>,
    step: f64,
    frac: f64,
    cur: [f32; 2],
    next: [f32; 2],
    max_lag: usize,
}

impl MicReader {
    fn new(m: MicIn, out_rate: u32) -> Self {
        Self {
            cons: m.cons,
            step: m.rate as f64 / out_rate.max(1) as f64,
            frac: 0.0,
            cur: [0.0; 2],
            next: [0.0; 2],
            max_lag: (m.rate * MIC_MAX_LAG_MS / 1000) as usize * 2,
        }
    }
    fn begin_block(&mut self) {
        let avail = self.cons.slots();
        if avail > self.max_lag {
            let drop = (avail - self.max_lag / 2) & !1;
            if let Ok(c) = self.cons.read_chunk(drop) {
                c.commit_all();
            }
        }
    }
    #[inline]
    fn next_frame(&mut self) -> [f32; 2] {
        while self.frac >= 1.0 {
            self.cur = self.next;
            if self.cons.slots() >= 2 {
                let l = self.cons.pop().unwrap_or(0.0);
                let r = self.cons.pop().unwrap_or(0.0);
                self.next = [l, r];
            }
            self.frac -= 1.0;
        }
        let t = self.frac as f32;
        self.frac += self.step;
        [
            self.cur[0] + (self.next[0] - self.cur[0]) * t,
            self.cur[1] + (self.next[1] - self.cur[1]) * t,
        ]
    }
}

fn build_output<T>(
    dev: &cpal::Device,
    config: &StreamConfig,
    mut cons: Consumer<f32>,
    mic: Option<MicIn>,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = config.channels as usize;
    let mut mic = mic.map(|m| MicReader::new(m, config.sample_rate.0));
    let err_shared = shared.clone();
    let cb_shared = shared;
    dev.build_output_stream(
        config,
        move |data: &mut [T], _| {
            let frames = data.len() / ch.max(1);
            let avail = cons.slots() & !1;
            let n = avail.min(frames * 2);
            let chunk = cons.read_chunk(n).ok();
            let (a, b) = chunk.as_ref().map(|c| c.as_slices()).unwrap_or((&[], &[]));
            let mut it = a.iter().chain(b.iter()).copied();
            if let Some(m) = mic.as_mut() {
                m.begin_block();
            }
            // Ses efekti çalarken fiziksel mikrofon geçici olarak susturulsun mu?
            let suppress_mic = cb_shared.mute_mic_on_play.load(Ordering::Relaxed)
                && cb_shared.active_voices.load(Ordering::Relaxed) > 0;
            for frame in data.chunks_mut(ch.max(1)) {
                let mut l = it.next().unwrap_or(0.0);
                let mut r = it.next().unwrap_or(0.0);
                if let Some(m) = mic.as_mut() {
                    let f = m.next_frame();
                    if !suppress_mic {
                        l = (l + f[0]).clamp(-1.0, 1.0);
                        r = (r + f[1]).clamp(-1.0, 1.0);
                    }
                }
                if ch == 1 {
                    frame[0] = T::from_sample((l + r) * 0.5);
                } else {
                    frame[0] = T::from_sample(l);
                    frame[1] = T::from_sample(r);
                    for s in frame.iter_mut().skip(2) {
                        *s = T::from_sample(0.0f32);
                    }
                }
            }
            drop(it);
            if let Some(c) = chunk {
                c.commit_all();
            }
        },
        move |e| {
            log::error!("Çıkış akışı hatası: {e}");
            err_shared.stream_error.store(true, Ordering::Relaxed);
        },
        None,
    )
    .map_err(|e| e.to_string())
}

fn open_input(dev: &cpal::Device, shared: Arc<Shared>) -> Result<(cpal::Stream, Consumer<f32>, u32), String> {
    let cfg = dev.default_input_config().map_err(|e| e.to_string())?;
    let fmt = cfg.sample_format();
    let config: StreamConfig = cfg.into();
    let rate = config.sample_rate.0;
    let cap = ((rate * MIC_BUFFER_MS / 1000) as usize * 2).max(2048) & !1;
    let (prod, cons) = RingBuffer::<f32>::new(cap);
    let stream = match fmt {
        SampleFormat::F32 => build_input::<f32>(dev, &config, prod, shared),
        SampleFormat::I16 => build_input::<i16>(dev, &config, prod, shared),
        SampleFormat::U16 => build_input::<u16>(dev, &config, prod, shared),
        SampleFormat::I32 => build_input::<i32>(dev, &config, prod, shared),
        other => return Err(format!("Desteklenmeyen mikrofon biçimi: {other:?}")),
    }?;
    Ok((stream, cons, rate))
}

fn build_input<T>(
    dev: &cpal::Device,
    config: &StreamConfig,
    mut prod: rtrb::Producer<f32>,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = config.channels as usize;
    let err_shared = shared.clone();
    dev.build_input_stream(
        config,
        move |data: &[T], _| {
            let mut peak = 0.0f32;
            let frames = data.len() / ch.max(1);
            let n = (frames * 2).min(prod.slots() & !1);
            let mut written = 0;
            if let Ok(mut chunk) = prod.write_chunk(n) {
                let (a, b) = chunk.as_mut_slices();
                let mut dst = a.iter_mut().chain(b.iter_mut());
                for frame in data.chunks(ch.max(1)) {
                    let l = f32::from_sample(frame[0]);
                    let r = if ch > 1 { f32::from_sample(frame[1]) } else { l };
                    peak = peak.max(l.abs()).max(r.abs());
                    if written < n {
                        if let (Some(dl), Some(dr)) = (dst.next(), dst.next()) {
                            *dl = l;
                            *dr = r;
                            written += 2;
                        }
                    }
                }
                drop(dst);
                chunk.commit(written);
            }
            let prev = f32::from_bits(shared.mic_level.load(Ordering::Relaxed));
            let lvl = peak.max(prev * 0.85);
            shared.mic_level.store(lvl.to_bits(), Ordering::Relaxed);
        },
        move |e| {
            log::error!("Mikrofon akışı hatası: {e}");
            err_shared.stream_error.store(true, Ordering::Relaxed);
        },
        None,
    )
    .map_err(|e| e.to_string())
}
