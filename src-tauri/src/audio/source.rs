//! Akış (streaming) tabanlı ses kaynakları. Dosyalar asla bütünüyle RAM'e
//! yüklenmez: `MediaSourceStream` diskten 64 KiB'lık parçalar halinde okur,
//! her çağrıda yalnızca bir paket çözülür.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use super::opus::OggOpusSource;

/// Diskten okunan parça boyutu.
pub const READ_CHUNK: usize = 64 * 1024;

pub trait SampleSource: Send {
    fn sample_rate(&self) -> u32;
    #[allow(dead_code)]
    fn channels(&self) -> usize;
    /// Sonraki çözülmüş bloğu `(interleaved_f32, sample_rate, channels)` döndürür; dosya sonunda `None`.
    fn next_block(&mut self) -> Result<Option<(&[f32], u32, usize)>, String>;
    /// Biliniyorsa toplam süre (saniye).
    fn duration(&self) -> Option<f32>;
}

pub fn open_source(path: &Path) -> Result<Box<dyn SampleSource>, String> {
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    if ext == "opus" {
        return Ok(Box::new(OggOpusSource::open(path)?));
    }
    match SymphoniaSource::open(path, &ext) {
        Ok(s) => Ok(Box::new(s)),
        // .ogg uzantılı dosyalar Opus içerebilir.
        Err(e) if ext == "ogg" => OggOpusSource::open(path).map(|s| Box::new(s) as Box<dyn SampleSource>).map_err(|_| e),
        Err(e) => Err(e),
    }
}

pub struct SymphoniaSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    buf: Option<SampleBuffer<f32>>,
    rate: u32,
    channels: usize,
    duration: Option<f32>,
}

impl SymphoniaSource {
    pub fn open(path: &Path, ext: &str) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("Dosya açılamadı: {e}"))?;
        let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions { buffer_len: READ_CHUNK });
        let mut hint = Hint::new();
        if !ext.is_empty() {
            hint.with_extension(ext);
        }
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions { enable_gapless: true, ..Default::default() }, &MetadataOptions::default())
            .map_err(|e| format!("Biçim tanınamadı: {e}"))?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or("Ses izi bulunamadı")?;
        let params = track.codec_params.clone();
        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|e| format!("Codec desteklenmiyor: {e}"))?;
        let rate = params.sample_rate.unwrap_or(48_000);
        let channels = params.channels.map(|c| c.count()).unwrap_or(2).max(1);
        let duration = params.n_frames.map(|n| n as f32 / rate as f32);
        Ok(Self { track_id: track.id, format, decoder, buf: None, rate, channels, duration })
    }
}

impl SampleSource for SymphoniaSource {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn channels(&self) -> usize {
        self.channels
    }
    fn duration(&self) -> Option<f32> {
        self.duration
    }

    fn next_block(&mut self) -> Result<Option<(&[f32], u32, usize)>, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(SymError::ResetRequired) => return Ok(None),
                Err(e) => return Err(format!("Okuma hatası: {e}")),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let frames = decoded.capacity() as u64;
                    if decoded.frames() == 0 {
                        continue;
                    }
                    let need_new = match &self.buf {
                        Some(b) => b.capacity() < decoded.capacity() * spec.channels.count(),
                        None => true,
                    };
                    if need_new {
                        self.buf = Some(SampleBuffer::<f32>::new(frames, spec));
                    }
                    self.rate = spec.rate;
                    self.channels = spec.channels.count().max(1);
                    let rate = self.rate;
                    let ch = self.channels;
                    let buf = self.buf.as_mut().unwrap();
                    buf.copy_interleaved_ref(decoded);
                    return Ok(Some((buf.samples(), rate, ch)));
                }
                // Bozuk paketleri atla, oynatmayı kesme.
                Err(SymError::DecodeError(_)) => continue,
                Err(SymError::IoError(_)) => return Ok(None),
                Err(e) => return Err(format!("Çözme hatası: {e}")),
            }
        }
    }
}
