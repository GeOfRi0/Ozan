//! Mikser / decoder thread'i.
//!
//! Tek bir arka plan thread'i ("ozan-decoder") aktif sesleri diskten küçük
//! paketler halinde çözer, 48 kHz stereo'da karıştırır ve sonucu her çıkış
//! cihazı için ayrı **kilitsiz SPSC halka tampona** (`rtrb`) yazar. Ses kartı
//! callback'leri bu tamponlardan kilit almadan okur.
//!
//! Kurallar:
//! - Aynı anda en fazla 5 ses (`MAX_CONCURRENT_VOICES = 5`) çalabilir.
//! - Çalan mevcut seslerin ses seviyesi kısılmaz (ducking uygulanmaz).
//! - 5 ses çalarken 6. bir ses tetiklendiğinde, ilk başlatılan en eski ses
//!   anında kesilir (FIFO hard-cut) ve yeni ses başlatılır.
//! - Tekrar modu: `ReplayMode::Restart` (Baştan Başlat) veya `ReplayMode::Overlap` (Üst Üste Bindir).

use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use ozan_shm::{FLAG_LOCAL_PREVIEW_ONLY, FLAG_MUTE_PHYSICAL_MIC, FLAG_PRODUCER_ACTIVE};
use rtrb::Producer;
use serde::Serialize;

use crate::library::ReplayMode;

use super::resample::{to_stereo, RubatoFormatAdapter, StereoResampler};
use super::source::SampleSource;
use super::Shared;

pub const MIX_RATE: u32 = 48_000;
pub const MAX_CONCURRENT_VOICES: usize = 5;
/// Bir karıştırma adımı: 5 ms (48 kHz'de 240 stereo kare).
const CHUNK_FRAMES: usize = 240;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceProgress {
    pub id: String,
    /// Saniye cinsinden anlık konum.
    pub position: f32,
    /// Saniye cinsinden toplam (veya kırpma bitiş) süresi.
    pub duration: f32,
    /// 0.0 ..= 1.0 ilerleme oranı.
    pub progress: f32,
}

pub enum MixCmd {
    Play {
        id: String,
        source: Box<dyn SampleSource>,
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
    #[allow(dead_code)]
    Stop(String),
    StopAll,
    SetMonitor(Option<Sink>),
    SetBroadcast(Option<Sink>),
    Shutdown,
}

/// Bir çıkış cihazına giden halka tamponun üretici ucu.
pub struct Sink {
    prod: Producer<f32>,
    rs: StereoResampler,
    scratch: Vec<f32>,
    out_rate: u32,
}

impl Sink {
    pub fn new(prod: Producer<f32>, out_rate: u32) -> Self {
        Self {
            prod,
            rs: StereoResampler::new(MIX_RATE, out_rate),
            scratch: Vec::with_capacity(4096),
            out_rate,
        }
    }

    fn needed_slots(&self) -> usize {
        (CHUNK_FRAMES as u64 * self.out_rate as u64 / MIX_RATE as u64) as usize * 2 + 8
    }

    fn has_room(&self) -> bool {
        self.prod.slots() >= self.needed_slots()
    }

    fn push(&mut self, mix: &[f32]) {
        self.scratch.clear();
        self.rs.process(mix, &mut self.scratch);
        let n = self.scratch.len().min(self.prod.slots()) & !1; // stereo hizası
        if n == 0 {
            return;
        }
        if let Ok(chunk) = self.prod.write_chunk_uninit(n) {
            chunk.fill_from_iter(self.scratch[..n].iter().copied());
        }
    }
}

struct Voice {
    seq: u64,
    id: String,
    src: Box<dyn SampleSource>,
    gain: f32,
    rs: StereoResampler,
    src_rate: u32,
    pending: Vec<f32>,
    pos: usize,
    tmp: Vec<f32>,
    done: bool,
    /// `MIX_RATE` (48 kHz) cinsinden başlangıçta atlanacak örnek çifti (kare) sayısı.
    skip_frames_left: u64,
    /// `MIX_RATE` cinsinden başlangıç ofseti (saniye hesabı için).
    start_frame_48k: u64,
    /// `MIX_RATE` cinsinden şimdiye kadar çıkışa yazılan kare sayısı.
    emitted_frames_48k: u64,
    /// `MIX_RATE` cinsinden maksimum çalınacak kare sayısı (`end_sec` varsa).
    max_emit_frames_48k: Option<u64>,
    /// Toplam süre (saniye).
    total_duration: f32,
    /// Kırpma aralığı sonu (varsa).
    end_sec: Option<f32>,
}

impl Voice {
    fn new(
        seq: u64,
        id: String,
        src: Box<dyn SampleSource>,
        gain: f32,
        total_duration: f32,
        start_sec: Option<f32>,
        end_sec: Option<f32>,
    ) -> Self {
        let src_rate = src.sample_rate();
        let start_s = start_sec.unwrap_or(0.0).max(0.0);
        let start_frame_48k = (start_s as f64 * MIX_RATE as f64).round() as u64;
        let max_emit_frames_48k = end_sec.and_then(|e| {
            if e > start_s {
                Some(((e - start_s) as f64 * MIX_RATE as f64).round() as u64)
            } else {
                None
            }
        });
        Self {
            seq,
            id,
            src,
            gain,
            rs: StereoResampler::new(src_rate, MIX_RATE),
            src_rate,
            pending: Vec::with_capacity(8192),
            pos: 0,
            tmp: Vec::with_capacity(8192),
            done: false,
            skip_frames_left: start_frame_48k,
            start_frame_48k,
            emitted_frames_48k: 0,
            max_emit_frames_48k,
            total_duration: total_duration.max(0.0),
            end_sec,
        }
    }

    fn progress(&self) -> VoiceProgress {
        let pos_sec = (self.start_frame_48k + self.emitted_frames_48k) as f32 / MIX_RATE as f32;
        let start_s = self.start_frame_48k as f32 / MIX_RATE as f32;
        let end_s = self.end_sec.unwrap_or(self.total_duration).max(pos_sec);
        let span = (end_s - start_s).max(0.001);
        let p = ((pos_sec - start_s) / span).clamp(0.0, 1.0);
        VoiceProgress {
            id: self.id.clone(),
            position: pos_sec,
            duration: end_s,
            progress: p,
        }
    }

    /// `out` içine (toplayarak, ducking olmadan) yazar. Ses bittiyse `false` döner.
    fn mix_into(&mut self, out: &mut [f32]) -> bool {
        let mut written = 0;
        while written < out.len() {
            if let Some(max_f) = self.max_emit_frames_48k {
                if self.emitted_frames_48k >= max_f {
                    self.done = true;
                    return written > 0;
                }
            }
            if self.pos >= self.pending.len() {
                if self.done {
                    return written > 0;
                }
                self.pending.clear();
                self.pos = 0;
                let Voice {
                    src,
                    tmp,
                    rs,
                    pending,
                    src_rate,
                    done,
                    ..
                } = self;
                match src.next_block() {
                    Ok(Some((block, rate, ch))) => {
                        if rate != *src_rate {
                            *src_rate = rate;
                            *rs = StereoResampler::new(rate, MIX_RATE);
                        }
                        tmp.clear();
                        to_stereo(block, ch, tmp);
                        rs.process(tmp, pending);
                    }
                    Ok(None) => *done = true,
                    Err(e) => {
                        log::warn!("Oynatma hatası: {e}");
                        *done = true;
                    }
                }
                // Başlangıç kırpması (start_sec) varsa o kısma kadar örnekleri hızlıca atla.
                if self.skip_frames_left > 0 && !self.pending.is_empty() {
                    let avail_frames = (self.pending.len() / 2) as u64;
                    let skip = self.skip_frames_left.min(avail_frames);
                    self.pos = (skip as usize) * 2;
                    self.skip_frames_left -= skip;
                }
                continue;
            }

            let mut avail_samples = self.pending.len() - self.pos;
            if let Some(max_f) = self.max_emit_frames_48k {
                let rem_frames = max_f.saturating_sub(self.emitted_frames_48k) as usize;
                avail_samples = avail_samples.min(rem_frames * 2);
            }
            let n = (out.len() - written).min(avail_samples) & !1;
            if n == 0 {
                self.done = true;
                return written > 0;
            }
            let g = self.gain;
            for (o, s) in out[written..written + n].iter_mut().zip(&self.pending[self.pos..self.pos + n]) {
                *o += *s * g;
            }
            written += n;
            self.pos += n;
            self.emitted_frames_48k += (n / 2) as u64;
        }
        true
    }
}

pub fn run(rx: Receiver<MixCmd>, shared: Arc<Shared>) {
    let mut voices: Vec<Voice> = Vec::new();
    let mut monitor: Option<Sink> = None;
    let mut broadcast: Option<Sink> = None;
    let mut mix = vec![0.0f32; CHUNK_FRAMES * 2];
    let mut shm_adapter = RubatoFormatAdapter::new(MIX_RATE, 48_000, 2);
    let mut shm_scratch: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES * 4);
    let mut next_seq: u64 = 1;
    let mut prev_active: usize = 0;

    if let Some(shm) = shared.shm_block() {
        shm.flags.fetch_or(FLAG_PRODUCER_ACTIVE, Ordering::Release);
    }

    let mut handle = |cmd: MixCmd,
                      voices: &mut Vec<Voice>,
                      monitor: &mut Option<Sink>,
                      broadcast: &mut Option<Sink>|
     -> bool {
        match cmd {
            MixCmd::Play {
                id,
                source,
                gain,
                replay_mode,
                total_duration,
                start_sec,
                end_sec,
            } => {
                // Tekrar modu "Baştan Başlat" ise aynı sesin mevcut örneklerini kes.
                if replay_mode == ReplayMode::Restart {
                    voices.retain(|v| v.id != id);
                }
                // 5 ses çalarken 6. bir ses tetiklenirse en eski başlatılan sesi anında kes (FIFO hard-cut).
                while voices.len() >= MAX_CONCURRENT_VOICES {
                    if let Some((oldest_idx, _)) = voices.iter().enumerate().min_by_key(|(_, v)| v.seq) {
                        voices.remove(oldest_idx);
                    } else {
                        break;
                    }
                }
                let seq = next_seq;
                next_seq = next_seq.wrapping_add(1);
                voices.push(Voice::new(seq, id, source, gain, total_duration, start_sec, end_sec));
            }
            MixCmd::SetVoiceGain { id, gain } => {
                for v in voices.iter_mut() {
                    if v.id == id {
                        v.gain = gain;
                    }
                }
            }
            MixCmd::Stop(id) => voices.retain(|v| v.id != id),
            MixCmd::StopAll => {
                voices.clear();
                if let Some(shm) = shared.shm_block() {
                    shm.clear();
                }
            }
            MixCmd::SetMonitor(s) => *monitor = s,
            MixCmd::SetBroadcast(s) => *broadcast = s,
            MixCmd::Shutdown => return true,
        }
        false
    };

    loop {
        let cur_active = voices.len();
        shared.active_voices.store(cur_active, Ordering::Relaxed);
        if (prev_active == 0) != (cur_active == 0) {
            crate::input::set_ptt_held(cur_active > 0);
            prev_active = cur_active;
        }
        if let Some(shm) = shared.shm_block() {
            let mute_mic = shared.mute_mic_on_play.load(Ordering::Relaxed) && cur_active > 0;
            if mute_mic {
                shm.flags.fetch_or(FLAG_MUTE_PHYSICAL_MIC, Ordering::Release);
            } else {
                shm.flags.fetch_and(!FLAG_MUTE_PHYSICAL_MIC, Ordering::Release);
            }
            if shared.local_preview.load(Ordering::Relaxed) {
                shm.flags.fetch_or(FLAG_LOCAL_PREVIEW_ONLY, Ordering::Release);
            } else {
                shm.flags.fetch_and(!FLAG_LOCAL_PREVIEW_ONLY, Ordering::Release);
            }
        }
        shared.update_progress(voices.iter().map(|v| v.progress()).collect());

        if voices.is_empty() {
            // Boşta: komut gelene kadar uyu (CPU ~0).
            match rx.recv() {
                Ok(cmd) => {
                    if handle(cmd, &mut voices, &mut monitor, &mut broadcast) {
                        if let Some(shm) = shared.shm_block() {
                            shm.flags
                                .fetch_and(!(FLAG_PRODUCER_ACTIVE | FLAG_MUTE_PHYSICAL_MIC), Ordering::Release);
                        }
                        crate::input::set_ptt_held(false);
                        return;
                    }
                }
                Err(_) => {
                    if let Some(shm) = shared.shm_block() {
                        shm.flags
                            .fetch_and(!(FLAG_PRODUCER_ACTIVE | FLAG_MUTE_PHYSICAL_MIC), Ordering::Release);
                    }
                    crate::input::set_ptt_held(false);
                    return;
                }
            }
            continue;
        }
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    if handle(cmd, &mut voices, &mut monitor, &mut broadcast) {
                        if let Some(shm) = shared.shm_block() {
                            shm.flags
                                .fetch_and(!(FLAG_PRODUCER_ACTIVE | FLAG_MUTE_PHYSICAL_MIC), Ordering::Release);
                        }
                        crate::input::set_ptt_held(false);
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if let Some(shm) = shared.shm_block() {
                        shm.flags
                            .fetch_and(!(FLAG_PRODUCER_ACTIVE | FLAG_MUTE_PHYSICAL_MIC), Ordering::Release);
                    }
                    crate::input::set_ptt_held(false);
                    return;
                }
            }
        }
        if voices.is_empty() {
            continue;
        }

        let local_preview = shared.local_preview.load(Ordering::Relaxed);
        let send_broadcast = !local_preview && broadcast.is_some();
        let send_shm = !local_preview && shared.shm_block().is_some();

        // Zamanlama: birincil saat kulaklık çıkışıdır; yoksa yayın çıkışı veya APO paylaşımlı bellek saati.
        let ready = match (&monitor, send_broadcast, send_shm) {
            (Some(m), _, _) => m.has_room(),
            (None, true, _) => broadcast.as_ref().map(|b| b.has_room()).unwrap_or(false),
            (None, false, true) => {
                std::thread::sleep(Duration::from_millis(5));
                shared
                    .shm_block()
                    .map(|b| b.available_write() >= CHUNK_FRAMES * 4)
                    .unwrap_or(false)
            }
            (None, false, false) => {
                voices.clear();
                continue;
            }
        };
        if !ready {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }

        mix.iter_mut().for_each(|s| *s = 0.0);
        voices.retain_mut(|v| v.mix_into(&mut mix));
        let vol = f32::from_bits(shared.volume.load(Ordering::Relaxed));
        for s in mix.iter_mut() {
            *s = (*s * vol).clamp(-1.0, 1.0);
        }
        if let Some(m) = monitor.as_mut() {
            m.push(&mix);
        }
        if send_broadcast {
            if let Some(b) = broadcast.as_mut() {
                b.push(&mix);
            }
        }
        // APO Paylaşımlı Bellek (`OzanAudioSharedMem_v1`) enjeksiyonu:
        // Hedef mikrofonun örnekleme hızına (44.1 kHz, 48 kHz, Bluetooth 16 kHz Mono vb.)
        // `rubato` ile dönüştürerek kilitsiz SPSC halka tampona yaz.
        if send_shm {
            if let Some(shm) = shared.shm_block() {
                let dst_rate = shm.apo_sample_rate.load(Ordering::Acquire).max(8_000);
                let dst_ch = shm.apo_channels.load(Ordering::Acquire).clamp(1, 16);
                shm_adapter.ensure_target_format(dst_rate, dst_ch);
                shm_adapter.convert(&mix, &mut shm_scratch);
                shm.push_samples(&shm_scratch);
            }
        }
        shared.touch();
    }
}
