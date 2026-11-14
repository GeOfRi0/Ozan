//! Saf Rust Ogg-Opus kaynağı (`opus-pure`). libopus / C derleyicisi gerektirmez.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use opus_pure::{OggOpusReader, OggPacket, OpusDecoder, Trim, MAX_PACKET_SAMPLES};

use super::source::{SampleSource, READ_CHUNK};

const OPUS_RATE: i32 = 48_000;

pub struct OggOpusSource {
    reader: OggOpusReader<BufReader<File>>,
    decoder: OpusDecoder,
    trim: Trim,
    packet: OggPacket,
    block: Vec<f32>,
    out: Vec<f32>,
    channels: usize,
}

impl OggOpusSource {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("Dosya açılamadı: {e}"))?;
        let reader = OggOpusReader::new(BufReader::with_capacity(READ_CHUNK, file))
            .map_err(|e| format!("Opus başlığı okunamadı: {e}"))?;
        let head = reader.head().clone();
        let channels = head.channel_count as usize;
        let decoder = head.decoder(OPUS_RATE).map_err(|e| format!("Opus çözücü oluşturulamadı: {e}"))?;
        let trim = Trim::new(&head, OPUS_RATE, channels).map_err(|e| format!("Opus: {e}"))?;
        Ok(Self {
            reader,
            decoder,
            trim,
            packet: OggPacket::default(),
            block: vec![0.0; MAX_PACKET_SAMPLES * channels.max(1)],
            out: Vec::with_capacity(MAX_PACKET_SAMPLES * channels.max(1)),
            channels: channels.max(1),
        })
    }
}

impl SampleSource for OggOpusSource {
    fn sample_rate(&self) -> u32 {
        OPUS_RATE as u32
    }
    fn channels(&self) -> usize {
        self.channels
    }
    fn duration(&self) -> Option<f32> {
        None
    }

    fn next_block(&mut self) -> Result<Option<(&[f32], u32, usize)>, String> {
        loop {
            match self.reader.read_packet_into(&mut self.packet) {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(e) => return Err(format!("Opus okuma hatası: {e}")),
            }
            let n = match self.decoder.decode(&self.packet.data, MAX_PACKET_SAMPLES, &mut self.block) {
                Ok(n) => n,
                Err(_) => continue, // bozuk paketi atla
            };
            let kept = self.trim.keep(&self.packet, &self.block[..n * self.channels]);
            if kept.is_empty() {
                continue;
            }
            self.out.clear();
            self.out.extend_from_slice(kept);
            return Ok(Some((&self.out, OPUS_RATE as u32, self.channels)));
        }
    }
}
