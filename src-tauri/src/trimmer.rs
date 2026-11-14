//! Ses kırpma (trimming) ve dalga formu (waveform) önizleme modülü.
//!
//! Uzun ses dosyalarında bile belleği şişirmemek için hem dalga formu analizi
//! hem de kalıcı kırpma işlemi akış (streaming) tabanlıdır.

use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use serde::Serialize;

use crate::audio::resample::to_stereo;
use crate::audio::source::open_source;
use crate::library::{new_id, sanitize};

const TARGET_PEAK: f32 = 0.891_250_9; // -1 dBFS

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveformData {
    pub peaks: Vec<f32>,
    pub duration: f32,
}

/// Ses dosyasını akış halinde okuyarak `buckets` adet tepe noktası ve kesin süreyi hesaplar.
pub fn compute_waveform(path: &Path, buckets: usize) -> Result<WaveformData, String> {
    let buckets = buckets.clamp(32, 1024);
    let mut src = open_source(path)?;

    // İlk geçişte toplam kare sayısını öğrenelim (veya örnekleri akan özet kovalarında toplayalım).
    // Tek geçişli dinamik özetleme: her bloktan gelen tepe değerlerini 4096'ya kadar ince kovada
    // biriktirip doldukça 2'ye katlayarak sıkıştırırız. Böylece dosya 3 saat bile olsa
    // tek geçişte ve sabit birkaç KB RAM ile kesin dalga formu çıkar!
    let mut fine_peaks: Vec<f32> = Vec::with_capacity(4096);
    let mut frames_per_fine: u64 = 256;
    let mut cur_max: f32 = 0.0;
    let mut cur_frames: u64 = 0;
    let mut total_frames: u64 = 0;
    let mut last_rate = src.sample_rate().max(1);

    while let Some((block, rate, ch)) = src.next_block()? {
        last_rate = rate.max(1);
        let ch = ch.max(1);
        for frame in block.chunks(ch) {
            let mut fmax = 0.0f32;
            for &s in frame {
                let a = s.abs();
                if a.is_finite() && a > fmax {
                    fmax = a;
                }
            }
            if fmax > cur_max {
                cur_max = fmax;
            }
            cur_frames += 1;
            total_frames += 1;
            if cur_frames >= frames_per_fine {
                fine_peaks.push(cur_max);
                cur_max = 0.0;
                cur_frames = 0;
                if fine_peaks.len() >= 4096 {
                    // İkişerli birleştirerek çözünürlüğü yarıya indir
                    let mut compacted = Vec::with_capacity(2048);
                    for pair in fine_peaks.chunks(2) {
                        let m = pair.iter().copied().fold(0.0f32, f32::max);
                        compacted.push(m);
                    }
                    fine_peaks = compacted;
                    frames_per_fine *= 2;
                }
            }
        }
    }
    if cur_frames > 0 {
        fine_peaks.push(cur_max);
    }

    let duration = total_frames as f32 / last_rate as f32;
    if fine_peaks.is_empty() {
        return Ok(WaveformData {
            peaks: vec![0.0; buckets],
            duration: 0.0,
        });
    }

    // `fine_peaks` dizisini istenen `buckets` uzunluğuna yeniden örnekle
    let mut out = Vec::with_capacity(buckets);
    let n = fine_peaks.len();
    for i in 0..buckets {
        let start = i * n / buckets;
        let end = ((i + 1) * n / buckets).max(start + 1).min(n);
        let m = fine_peaks[start..end].iter().copied().fold(0.0f32, f32::max);
        out.push(m.clamp(0.0, 1.0));
    }

    // Görsel netlik için en yüksek tepeye göre normalize et
    let global_max = out.iter().copied().fold(0.0f32, f32::max);
    if global_max > 1e-4 {
        for v in &mut out {
            *v = (*v / global_max).clamp(0.02, 1.0);
        }
    }

    Ok(WaveformData { peaks: out, duration })
}

/// Seçilen `[start_sec, end_sec]` aralığını kalıcı bir `.wav` dosyası olarak yazar ve
/// `(yeni_dosya_adı, yeni_gain, yeni_süre)` döndürür.
pub fn trim_to_file(
    sounds_dir: &Path,
    old_file: &str,
    start_sec: f32,
    end_sec: f32,
    normalized: bool,
) -> Result<(String, f32, f32), String> {
    if end_sec <= start_sec || start_sec < 0.0 {
        return Err("Geçersiz kırpma aralığı: bitiş süresi başlangıçtan büyük olmalıdır.".into());
    }

    let src_path = sounds_dir.join(old_file);
    let mut src = open_source(&src_path)?;
    let sample_rate = src.sample_rate().max(8_000);
    let channels: u16 = 2; // Stereo PCM WAV olarak kaydet

    let tmp_name = format!(".trim_{}.wav", new_id("t"));
    let tmp_path = sounds_dir.join(&tmp_name);

    let mut peak = 0.0f32;
    let mut written_frames: u64 = 0;

    let write_res: Result<(), String> = (|| {
        let file = File::create(&tmp_path).map_err(|e| format!("Geçici dosya oluşturulamadı: {e}"))?;
        let mut w = BufWriter::with_capacity(64 * 1024, file);

        // 44 baytlık WAV başlığı yer tutucusu
        write_wav_header(&mut w, sample_rate, channels, 0)?;

        let mut cur_frame: u64 = 0;
        let mut stereo_buf: Vec<f32> = Vec::with_capacity(4096);
        let mut pcm_bytes: Vec<u8> = Vec::with_capacity(8192);

        while let Some((block, b_rate, b_ch)) = src.next_block()? {
            let rate = b_rate.max(1);
            let start_frame = (start_sec as f64 * rate as f64).round() as u64;
            let end_frame = (end_sec as f64 * rate as f64).round() as u64;
            if cur_frame >= end_frame {
                break;
            }
            stereo_buf.clear();
            to_stereo(block, b_ch.max(1), &mut stereo_buf);
            let block_frames = (stereo_buf.len() / 2) as u64;
            let block_end = cur_frame + block_frames;

            if block_end <= start_frame {
                cur_frame = block_end;
                continue;
            }

            let skip_in_block = start_frame.saturating_sub(cur_frame) as usize;
            let take_until_in_block = (end_frame - cur_frame).min(block_frames) as usize;
            if take_until_in_block > skip_in_block {
                let slice = &stereo_buf[skip_in_block * 2..take_until_in_block * 2];
                pcm_bytes.clear();
                for &s in slice {
                    let a = s.abs();
                    if a.is_finite() && a > peak {
                        peak = a;
                    }
                    let clamped = s.clamp(-1.0, 1.0);
                    let q = (clamped * 32767.0).round() as i16;
                    pcm_bytes.extend_from_slice(&q.to_le_bytes());
                }
                w.write_all(&pcm_bytes).map_err(|e| format!("WAV yazma hatası: {e}"))?;
                written_frames += (take_until_in_block - skip_in_block) as u64;
            }

            cur_frame = block_end;
            if cur_frame >= end_frame {
                break;
            }
        }

        if written_frames == 0 {
            return Err("Seçilen aralıkta ses verisi bulunamadı.".into());
        }

        // Gerçek boyutlarla WAV başlığını güncelle
        let data_bytes = (written_frames * channels as u64 * 2).min(u32::MAX as u64) as u32;
        w.flush().map_err(|e| e.to_string())?;
        w.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        write_wav_header(&mut w, sample_rate, channels, data_bytes)?;
        w.flush().map_err(|e| e.to_string())?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    // Hedef dosya adını belirle (.wav uzantılı)
    let old_stem = Path::new(old_file)
        .file_stem()
        .map(|s| sanitize(&s.to_string_lossy()))
        .unwrap_or_else(|| "ses".into());
    let stem = if old_stem.is_empty() { "ses".to_string() } else { old_stem };
    let target_file = if old_file.to_ascii_lowercase().ends_with(".wav") {
        old_file.to_string()
    } else {
        let mut cand = format!("{stem}.wav");
        let mut n = 2;
        while sounds_dir.join(&cand).exists() && !cand.eq_ignore_ascii_case(old_file) {
            cand = format!("{stem} ({n}).wav");
            n += 1;
        }
        cand
    };

    let target_path = sounds_dir.join(&target_file);
    fs::rename(&tmp_path, &target_path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        format!("Kırpılmış dosya kaydedilemedi: {e}")
    })?;

    if !target_file.eq_ignore_ascii_case(old_file) {
        let _ = fs::remove_file(&src_path);
    }

    let new_duration = written_frames as f32 / sample_rate as f32;
    let new_gain = if normalized {
        if peak > 1e-6 {
            TARGET_PEAK / peak
        } else {
            1.0
        }
    } else {
        1.0
    };

    Ok((target_file, new_gain, new_duration))
}

fn write_wav_header<W: Write>(w: &mut W, sample_rate: u32, channels: u16, data_size: u32) -> Result<(), String> {
    let bits_per_sample: u16 = 16;
    let block_align: u16 = channels * (bits_per_sample / 8);
    let byte_rate: u32 = sample_rate * block_align as u32;
    let riff_size: u32 = 36u32.saturating_add(data_size);

    w.write_all(b"RIFF").map_err(|e| e.to_string())?;
    w.write_all(&riff_size.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(b"WAVEfmt ").map_err(|e| e.to_string())?;
    w.write_all(&16u32.to_le_bytes()).map_err(|e| e.to_string())?; // PCM fmt chunk size
    w.write_all(&1u16.to_le_bytes()).map_err(|e| e.to_string())?; // AudioFormat = 1 (PCM)
    w.write_all(&channels.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&sample_rate.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&byte_rate.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&block_align.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&bits_per_sample.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(b"data").map_err(|e| e.to_string())?;
    w.write_all(&data_size.to_le_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}
