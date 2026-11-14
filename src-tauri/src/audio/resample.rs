//! `rubato` tabanlı gerçek zamanlı yeniden örnekleme (resampling) ve kanal eşitleme modülü.
//!
//! Farklı örnekleme hızları (44.1 kHz, 48 kHz, 96 kHz, Bluetooth 16 kHz Mono vb.)
//! ve kanal sayıları (1 Mono, 2 Stereo, Çok Kanallı) arasında ön-ayrılmış tamponlarla
//! (`rubato::Resampler::process_into_buffer`) sıfır bellek ayırma ile dönüşüm yapar.

use rubato::{FastFixedIn, Indexing, PolynomialDegree, Resampler};

const DEFAULT_CHUNK_FRAMES: usize = 4096;

pub struct StereoResampler {
    passthrough: bool,
    rubato: FastFixedIn<f32>,
    in_planar: Vec<Vec<f32>>,
    out_planar: Vec<Vec<f32>>,
}

impl StereoResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let in_rate = in_rate.max(1);
        let out_rate = out_rate.max(1);
        let ratio = (out_rate as f64) / (in_rate as f64);
        let rubato = FastFixedIn::new(
            ratio,
            2.0,
            PolynomialDegree::Cubic,
            DEFAULT_CHUNK_FRAMES,
            2,
        )
        .unwrap_or_else(|_| {
            FastFixedIn::new(1.0, 2.0, PolynomialDegree::Linear, DEFAULT_CHUNK_FRAMES, 2).unwrap()
        });
        let max_out = rubato.output_frames_max().max(DEFAULT_CHUNK_FRAMES * 2 + 64);
        Self {
            passthrough: in_rate == out_rate,
            rubato,
            in_planar: vec![vec![0.0f32; DEFAULT_CHUNK_FRAMES]; 2],
            out_planar: vec![vec![0.0f32; max_out]; 2],
        }
    }

    /// `input` interleaved stereo; sonuç `out` sonuna eklenir (`rubato::Resampler::process_into_buffer` kullanır).
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        let total_frames = input.len() / 2;
        if total_frames == 0 {
            return;
        }

        let mut offset_frames = 0usize;
        while offset_frames < total_frames {
            let chunk_frames = (total_frames - offset_frames).min(DEFAULT_CHUNK_FRAMES);
            for i in 0..chunk_frames {
                let src_idx = (offset_frames + i) * 2;
                self.in_planar[0][i] = input[src_idx];
                self.in_planar[1][i] = input[src_idx + 1];
            }

            let idx = Indexing {
                input_offset: 0,
                output_offset: 0,
                active_channels_mask: None,
                partial_len: Some(chunk_frames),
            };

            if let Ok((_, produced_frames)) =
                self.rubato
                    .process_into_buffer(&self.in_planar, &mut self.out_planar, Some(&idx))
            {
                out.reserve(produced_frames * 2);
                for f in 0..produced_frames {
                    out.push(self.out_planar[0][f]);
                    out.push(self.out_planar[1][f]);
                }
            }
            offset_frames += chunk_frames;
        }
    }
}

/// 48 kHz Stereo mikser çıkışını aktif mikrofonun örnekleme hızına (örn. 44.1 kHz,
/// 48 kHz, Bluetooth 16 kHz Mono) ve kanal sayısına `rubato` ile dönüştüren adaptör.
pub struct RubatoFormatAdapter {
    src_rate: u32,
    dst_rate: u32,
    dst_channels: u32,
    rubato: FastFixedIn<f32>,
    in_planar: Vec<Vec<f32>>,
    out_planar: Vec<Vec<f32>>,
}

impl RubatoFormatAdapter {
    pub fn new(src_rate: u32, dst_rate: u32, dst_channels: u32) -> Self {
        let src_rate = src_rate.max(8_000);
        let dst_rate = dst_rate.max(8_000);
        let dst_channels = dst_channels.clamp(1, 16);
        let ratio = (dst_rate as f64) / (src_rate as f64);
        let rubato = FastFixedIn::new(ratio, 4.0, PolynomialDegree::Cubic, 1024, 2)
            .unwrap_or_else(|_| {
                FastFixedIn::new(1.0, 2.0, PolynomialDegree::Linear, 1024, 2).unwrap()
            });
        let max_out = rubato.output_frames_max().max(4096);
        Self {
            src_rate,
            dst_rate,
            dst_channels,
            rubato,
            in_planar: vec![vec![0.0f32; 1024]; 2],
            out_planar: vec![vec![0.0f32; max_out]; 2],
        }
    }

    /// Hedef mikrofon örnekleme hızı veya kanal sayısı değiştiyse (`IMMNotificationClient`
    /// veya `LockForProcess` üzerinden) `rubato` oranını anında günceller.
    #[inline]
    pub fn ensure_target_format(&mut self, dst_rate: u32, dst_channels: u32) {
        let dst_rate = dst_rate.clamp(8_000, 384_000);
        let dst_channels = dst_channels.clamp(1, 16);
        if self.dst_rate != dst_rate {
            self.dst_rate = dst_rate;
            let ratio = (dst_rate as f64) / (self.src_rate as f64);
            let _ = self.rubato.set_resample_ratio(ratio, false);
        }
        self.dst_channels = dst_channels;
    }

    /// 48 kHz Stereo (`stereo_in`) dilimini hedef hız ve kanal sayısına dönüştürerek `out` içine yazar.
    pub fn convert(&mut self, stereo_in: &[f32], out: &mut Vec<f32>) {
        out.clear();
        let frames = (stereo_in.len() / 2).min(self.in_planar[0].len());
        if frames == 0 {
            return;
        }

        // Birebir 48 kHz Stereo ise hızlı yol
        if self.src_rate == self.dst_rate && self.dst_channels == 2 {
            out.extend_from_slice(&stereo_in[..frames * 2]);
            return;
        }

        for i in 0..frames {
            self.in_planar[0][i] = stereo_in[i * 2];
            self.in_planar[1][i] = stereo_in[i * 2 + 1];
        }

        let idx = Indexing {
            input_offset: 0,
            output_offset: 0,
            active_channels_mask: None,
            partial_len: Some(frames),
        };

        if let Ok((_, out_frames)) =
            self.rubato
                .process_into_buffer(&self.in_planar, &mut self.out_planar, Some(&idx))
        {
            let ch = self.dst_channels as usize;
            out.reserve(out_frames * ch);
            match ch {
                1 => {
                    // Bluetooth 16 kHz Mono vb.: Stereo -> Mono (`0.5 * (L + R)`)
                    for f in 0..out_frames {
                        out.push(0.5 * (self.out_planar[0][f] + self.out_planar[1][f]));
                    }
                }
                2 => {
                    for f in 0..out_frames {
                        out.push(self.out_planar[0][f]);
                        out.push(self.out_planar[1][f]);
                    }
                }
                n => {
                    for f in 0..out_frames {
                        out.push(self.out_planar[0][f]);
                        out.push(self.out_planar[1][f]);
                        for _ in 2..n {
                            out.push(0.0);
                        }
                    }
                }
            }
        }
    }
}

/// Herhangi bir kanal sayısını stereo'ya çevirir (sonuç `out` sonuna eklenir).
pub fn to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        2 => out.extend_from_slice(input),
        1 => {
            out.reserve(input.len() * 2);
            for &s in input {
                out.push(s);
                out.push(s);
            }
        }
        n if n > 2 => {
            out.reserve(input.len() / n * 2);
            for f in input.chunks_exact(n) {
                out.push(f[0]);
                out.push(f[1]);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_identical() {
        let mut r = StereoResampler::new(48_000, 48_000);
        let mut out = Vec::new();
        r.process(&[1.0, 2.0, 3.0, 4.0], &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn upsample_length() {
        let mut r = StereoResampler::new(24_000, 48_000);
        let input: Vec<f32> = (0..2000).map(|i| i as f32).collect();
        let mut out = Vec::new();
        r.process(&input[..1000], &mut out);
        r.process(&input[1000..], &mut out);
        let frames = out.len() / 2;
        assert!((frames as i64 - 2000).abs() <= 4, "frames={frames}");
    }

    #[test]
    fn rubato_bluetooth_16khz_mono_conversion() {
        let mut adapter = RubatoFormatAdapter::new(48_000, 16_000, 1);
        // 480 stereo kare (960 f32 örnek, 10 ms @ 48kHz) -> ~160 mono örnek (@ 16kHz)
        let stereo_in: Vec<f32> = (0..960).map(|i| if i % 2 == 0 { 0.4 } else { 0.6 }).collect();
        let mut mono_out = Vec::new();
        adapter.convert(&stereo_in, &mut mono_out);
        assert!(
            (mono_out.len() as i64 - 160).abs() <= 2,
            "16kHz Mono örnek sayısı ~160 olmalı, bulunan: {}",
            mono_out.len()
        );
    }
}
