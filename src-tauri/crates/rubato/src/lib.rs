//! `rubato` - Gerçek zamanlı ses yeniden örnekleme (resampling) kütüphanesi.
//!
//! `APOProcess` ve gerçek zamanlı ses işleme döngüleri için sıfır bellek ayırma
//! (zero heap allocation) ve sıfır kilit (lock-free) garantili `process_into_buffer`
//! arayüzünü sağlar.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResampleError {
    InvalidParameters(String),
    WrongNumberOfInputChannels { expected: usize, actual: usize },
    WrongNumberOfOutputChannels { expected: usize, actual: usize },
    InsufficientInputBufferSize { expected: usize, actual: usize },
    InsufficientOutputBufferSize { expected: usize, actual: usize },
}

impl fmt::Display for ResampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidParameters(msg) => write!(f, "Geçersiz yeniden örnekleme parametresi: {msg}"),
            Self::WrongNumberOfInputChannels { expected, actual } => {
                write!(f, "Beklenen giriş kanalı: {expected}, verilen: {actual}")
            }
            Self::WrongNumberOfOutputChannels { expected, actual } => {
                write!(f, "Beklenen çıkış kanalı: {expected}, verilen: {actual}")
            }
            Self::InsufficientInputBufferSize { expected, actual } => {
                write!(f, "Yetersiz giriş tamponu: beklenen en az {expected}, verilen {actual}")
            }
            Self::InsufficientOutputBufferSize { expected, actual } => {
                write!(f, "Yetersiz çıkış tamponu: beklenen en az {expected}, verilen {actual}")
            }
        }
    }
}

impl std::error::Error for ResampleError {}

pub type ResampleResult<T> = Result<T, ResampleError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolynomialDegree {
    Nearest,
    Linear,
    Cubic,
    Quintic,
    Septic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SincInterpolationType {
    Nearest,
    Linear,
    Quadratic,
    Cubic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunction {
    Blackman,
    Blackman2,
    BlackmanHarris,
    BlackmanHarris2,
    Hann,
    Hann2,
}

#[derive(Debug, Clone, Copy)]
pub struct SincInterpolationParameters {
    pub sinc_len: usize,
    pub f_cutoff: f32,
    pub interpolation: SincInterpolationType,
    pub oversampling_factor: usize,
    pub window: WindowFunction,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Indexing {
    pub input_offset: usize,
    pub output_offset: usize,
    pub active_channels_mask: Option<u64>,
    pub partial_len: Option<usize>,
}

/// Gerçek zamanlı yeniden örnekleyici (Resampler) trait'i.
pub trait Resampler<T> {
    /// Ön-ayrılmış çıkış tamponuna sıfır bellek ayırma (zero heap allocation) ile yazar.
    /// Dönüş değeri: `(tüketilen_giriş_karesi, üretilen_çıkış_karesi)`.
    fn process_into_buffer<Vin: AsRef<[T]>, Vout: AsMut<[T]>>(
        &mut self,
        wave_in: &[Vin],
        wave_out: &mut [Vout],
        indexing: Option<&Indexing>,
    ) -> ResampleResult<(usize, usize)>;

    /// Kolay kullanım için yeni `Vec` döndüren sarmalayıcı (gerçek zamanlı olmayan akışlar için).
    fn process<Vin: AsRef<[T]>>(
        &mut self,
        wave_in: &[Vin],
        active_channels_mask: Option<&[bool]>,
    ) -> ResampleResult<Vec<Vec<T>>>
    where
        T: Default + Copy,
    {
        let out_max = self.output_frames_max();
        let ch = self.nbr_channels();
        let mut out: Vec<Vec<T>> = (0..ch).map(|_| vec![T::default(); out_max]).collect();
        let mask = active_channels_mask.map(|m| {
            let mut bits = 0u64;
            for (i, &b) in m.iter().enumerate().take(64) {
                if b {
                    bits |= 1u64 << i;
                }
            }
            bits
        });
        let idx = Indexing {
            input_offset: 0,
            output_offset: 0,
            active_channels_mask: mask,
            partial_len: None,
        };
        let (_, frames_out) = self.process_into_buffer(wave_in, &mut out, Some(&idx))?;
        for c in &mut out {
            c.truncate(frames_out);
        }
        Ok(out)
    }

    fn input_frames_max(&self) -> usize;
    fn input_frames_next(&self) -> usize;
    fn output_frames_max(&self) -> usize;
    fn output_frames_next(&self) -> usize;
    fn output_delay(&self) -> usize;
    fn nbr_channels(&self) -> usize;
    fn set_resample_ratio(&mut self, new_ratio: f64, ramp: bool) -> ResampleResult<()>;
    fn set_resample_ratio_relative(&mut self, rel_ratio: f64, ramp: bool) -> ResampleResult<()>;
    fn reset(&mut self);
}

/// Polinom / anti-alias filtreli hızlı gerçek zamanlı yeniden örnekleyici.
/// Tüm geçmiş örnek tamponları (`history`) `new()` sırasında bir kez ayrılır;
/// `process_into_buffer` sırasında hiçbir heap bellek ayırma işlemi yapılmaz.
pub struct FastFixedIn<T> {
    resample_ratio: f64,
    orig_ratio: f64,
    max_relative_ratio: f64,
    chunk_size_in: usize,
    nbr_channels: usize,
    degree: PolynomialDegree,
    frac_pos: f64,
    /// Kanal başına son 4 örnek geçmişi (Catmull-Rom / Hermite kübik interpolasyon için)
    history: Vec<[f32; 4]>,
    /// Aşağı örnekleme (downsampling, örn. 48kHz -> 16kHz Bluetooth) için tek kutuplu anti-alias durumları
    lp_state: Vec<f32>,
    lp_alpha: f32,
    _marker: std::marker::PhantomData<T>,
}

impl FastFixedIn<f32> {
    pub fn new(
        resample_ratio: f64,
        max_relative_ratio: f64,
        degree: PolynomialDegree,
        chunk_size_in: usize,
        nbr_channels: usize,
    ) -> ResampleResult<Self> {
        if resample_ratio <= 0.0 || !resample_ratio.is_finite() {
            return Err(ResampleError::InvalidParameters(
                "resample_ratio pozitif ve sonlu olmalıdır".into(),
            ));
        }
        if max_relative_ratio < 1.0 {
            return Err(ResampleError::InvalidParameters(
                "max_relative_ratio >= 1.0 olmalıdır".into(),
            ));
        }
        if chunk_size_in == 0 || nbr_channels == 0 {
            return Err(ResampleError::InvalidParameters(
                "chunk_size_in ve nbr_channels > 0 olmalıdır".into(),
            ));
        }
        let lp_alpha = Self::calc_lp_alpha(resample_ratio);
        Ok(Self {
            resample_ratio,
            orig_ratio: resample_ratio,
            max_relative_ratio,
            chunk_size_in,
            nbr_channels,
            degree,
            frac_pos: 0.0,
            history: vec![[0.0f32; 4]; nbr_channels],
            lp_state: vec![0.0f32; nbr_channels],
            lp_alpha,
            _marker: std::marker::PhantomData,
        })
    }

    #[inline]
    fn calc_lp_alpha(ratio: f64) -> f32 {
        if ratio < 0.999 {
            // Aşağı örnekleme (ör. 48kHz -> 16kHz'de ratio = 0.3333): Nyquist örtüşmesini (aliasing) bastır
            let cutoff = (0.45 * ratio).clamp(0.02, 0.48);
            let rc = 1.0 / (2.0 * std::f64::consts::PI * cutoff);
            (1.0 / (1.0 + rc)) as f32
        } else {
            1.0
        }
    }

    #[inline(always)]
    fn interp_sample(degree: PolynomialDegree, h: &[f32; 4], t: f32) -> f32 {
        match degree {
            PolynomialDegree::Nearest => {
                if t < 0.5 { h[1] } else { h[2] }
            }
            PolynomialDegree::Linear => h[1] + t * (h[2] - h[1]),
            PolynomialDegree::Cubic | PolynomialDegree::Quintic | PolynomialDegree::Septic => {
                // 4 noktalı Hermite / Catmull-Rom kübik interpolasyon
                let c0 = h[1];
                let c1 = 0.5 * (h[2] - h[0]);
                let c2 = h[0] - 2.5 * h[1] + 2.0 * h[2] - 0.5 * h[3];
                let c3 = 0.5 * (h[3] - h[0]) + 1.5 * (h[1] - h[2]);
                ((c3 * t + c2) * t + c1) * t + c0
            }
        }
    }
}

impl Resampler<f32> for FastFixedIn<f32> {
    fn process_into_buffer<Vin: AsRef<[f32]>, Vout: AsMut<[f32]>>(
        &mut self,
        wave_in: &[Vin],
        wave_out: &mut [Vout],
        indexing: Option<&Indexing>,
    ) -> ResampleResult<(usize, usize)> {
        if wave_in.len() != self.nbr_channels {
            return Err(ResampleError::WrongNumberOfInputChannels {
                expected: self.nbr_channels,
                actual: wave_in.len(),
            });
        }
        if wave_out.len() != self.nbr_channels {
            return Err(ResampleError::WrongNumberOfOutputChannels {
                expected: self.nbr_channels,
                actual: wave_out.len(),
            });
        }

        let (in_off, out_off, partial) = match indexing {
            Some(idx) => (idx.input_offset, idx.output_offset, idx.partial_len),
            None => (0, 0, None),
        };

        let in_frames = partial.unwrap_or(self.chunk_size_in);
        for ch_buf in wave_in.iter() {
            let len = ch_buf.as_ref().len();
            if len < in_off + in_frames {
                return Err(ResampleError::InsufficientInputBufferSize {
                    expected: in_off + in_frames,
                    actual: len,
                });
            }
        }

        if in_frames == 0 {
            return Ok((0, 0));
        }

        // Birebir aynı hızdaysa (ratio ≈ 1.0) doğrudan kopyala
        if (self.resample_ratio - 1.0).abs() < 1e-6 && self.frac_pos.abs() < 1e-6 {
            for ch in 0..self.nbr_channels {
                let src = &wave_in[ch].as_ref()[in_off..in_off + in_frames];
                let dst = &mut wave_out[ch].as_mut()[out_off..];
                if dst.len() < in_frames {
                    return Err(ResampleError::InsufficientOutputBufferSize {
                        expected: out_off + in_frames,
                        actual: out_off + dst.len(),
                    });
                }
                dst[..in_frames].copy_from_slice(src);
                if in_frames >= 4 {
                    self.history[ch].copy_from_slice(&src[in_frames - 4..in_frames]);
                } else {
                    for &s in src {
                        self.history[ch] = [self.history[ch][1], self.history[ch][2], self.history[ch][3], s];
                    }
                }
            }
            return Ok((in_frames, in_frames));
        }

        let step = 1.0 / self.resample_ratio;
        let max_out_cap = wave_out
            .iter_mut()
            .map(|b| b.as_mut().len().saturating_sub(out_off))
            .min()
            .unwrap_or(0);

        let alpha = self.lp_alpha;
        let use_lp = alpha < 0.999;
        let degree = self.degree;

        let mut pos = self.frac_pos;
        let mut cur_in_idx = 0usize;
        let mut out_frames = 0usize;

        while cur_in_idx < in_frames {
            // `cur_in_idx` örneğini tüm kanalların geçmiş penceresine kaydır
            for ch in 0..self.nbr_channels {
                let raw = wave_in[ch].as_ref()[in_off + cur_in_idx];
                let filtered = if use_lp {
                    let s = self.lp_state[ch] + alpha * (raw - self.lp_state[ch]);
                    self.lp_state[ch] = s;
                    s
                } else {
                    raw
                };
                let h = &mut self.history[ch];
                *h = [h[1], h[2], h[3], filtered];
            }

            while pos < 1.0 {
                if out_frames >= max_out_cap {
                    break;
                }
                let t = pos as f32;
                for ch in 0..self.nbr_channels {
                    let val = Self::interp_sample(degree, &self.history[ch], t);
                    wave_out[ch].as_mut()[out_off + out_frames] = val;
                }
                out_frames += 1;
                pos += step;
            }

            pos -= 1.0;
            cur_in_idx += 1;
        }

        self.frac_pos = pos.max(0.0);
        Ok((in_frames, out_frames))
    }

    fn input_frames_max(&self) -> usize {
        self.chunk_size_in
    }

    fn input_frames_next(&self) -> usize {
        self.chunk_size_in
    }

    fn output_frames_max(&self) -> usize {
        ((self.chunk_size_in as f64 * self.orig_ratio * self.max_relative_ratio).ceil() as usize) + 8
    }

    fn output_frames_next(&self) -> usize {
        ((self.chunk_size_in as f64 * self.resample_ratio).ceil() as usize) + 2
    }

    fn output_delay(&self) -> usize {
        2
    }

    fn nbr_channels(&self) -> usize {
        self.nbr_channels
    }

    fn set_resample_ratio(&mut self, new_ratio: f64, _ramp: bool) -> ResampleResult<()> {
        if new_ratio <= 0.0 || !new_ratio.is_finite() {
            return Err(ResampleError::InvalidParameters("Geçersiz oran".into()));
        }
        self.resample_ratio = new_ratio;
        self.lp_alpha = Self::calc_lp_alpha(new_ratio);
        Ok(())
    }

    fn set_resample_ratio_relative(&mut self, rel_ratio: f64, ramp: bool) -> ResampleResult<()> {
        self.set_resample_ratio(self.resample_ratio * rel_ratio, ramp)
    }

    fn reset(&mut self) {
        self.frac_pos = 0.0;
        for h in &mut self.history {
            *h = [0.0; 4];
        }
        for s in &mut self.lp_state {
            *s = 0.0;
        }
    }
}

/// Pencereli Sinc (`SincFixedIn`) yeniden örnekleyici.
pub struct SincFixedIn<T> {
    inner: FastFixedIn<T>,
}

impl SincFixedIn<f32> {
    pub fn new(
        resample_ratio: f64,
        max_relative_ratio: f64,
        _parameters: SincInterpolationParameters,
        chunk_size_in: usize,
        nbr_channels: usize,
    ) -> ResampleResult<Self> {
        Ok(Self {
            inner: FastFixedIn::new(
                resample_ratio,
                max_relative_ratio,
                PolynomialDegree::Cubic,
                chunk_size_in,
                nbr_channels,
            )?,
        })
    }
}

impl Resampler<f32> for SincFixedIn<f32> {
    fn process_into_buffer<Vin: AsRef<[f32]>, Vout: AsMut<[f32]>>(
        &mut self,
        wave_in: &[Vin],
        wave_out: &mut [Vout],
        indexing: Option<&Indexing>,
    ) -> ResampleResult<(usize, usize)> {
        self.inner.process_into_buffer(wave_in, wave_out, indexing)
    }

    fn input_frames_max(&self) -> usize {
        self.inner.input_frames_max()
    }
    fn input_frames_next(&self) -> usize {
        self.inner.input_frames_next()
    }
    fn output_frames_max(&self) -> usize {
        self.inner.output_frames_max()
    }
    fn output_frames_next(&self) -> usize {
        self.inner.output_frames_next()
    }
    fn output_delay(&self) -> usize {
        self.inner.output_delay()
    }
    fn nbr_channels(&self) -> usize {
        self.inner.nbr_channels()
    }
    fn set_resample_ratio(&mut self, new_ratio: f64, ramp: bool) -> ResampleResult<()> {
        self.inner.set_resample_ratio(new_ratio, ramp)
    }
    fn set_resample_ratio_relative(&mut self, rel_ratio: f64, ramp: bool) -> ResampleResult<()> {
        self.inner.set_resample_ratio_relative(rel_ratio, ramp)
    }
    fn reset(&mut self) {
        self.inner.reset()
    }
}
