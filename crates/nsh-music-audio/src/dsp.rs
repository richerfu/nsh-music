use std::f32::consts::{FRAC_PI_2, PI};

/// Number of perceptual frequency bands exported for UI visualizers and retro stages.
pub const SPECTRUM_BANDS: usize = 16;
const FFT_SIZE: usize = 512;

/// Configuration for the audio DSP processor chain.
#[derive(Debug, Clone, Copy)]
pub struct DspConfig {
    pub sample_rate: u32,
    pub channels: u16,
    /// User volume in `0.0..=1.0`.
    pub volume: f32,
    pub muted: bool,
    /// Track/album ReplayGain adjustment in dB (0.0 disables gain offset).
    pub replay_gain_db: f32,
    /// Track peak amplitude (typically `0.0..=1.0+`) for anti-clipping protection.
    pub replay_gain_peak: f32,
    pub replay_gain_enabled: bool,
    /// Click-free volume ramp duration in milliseconds.
    pub ramp_ms: f32,
    /// Soft limiter knee threshold (`0.5..=0.98`).
    pub limiter_threshold: f32,
    /// Soft limiter hard ceiling (`<= 0.995`).
    pub limiter_ceiling: f32,
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            sample_rate: 44_100,
            channels: 2,
            volume: 0.85,
            muted: false,
            replay_gain_db: 0.0,
            replay_gain_peak: 1.0,
            replay_gain_enabled: true,
            ramp_ms: 15.0,
            limiter_threshold: 0.85,
            limiter_ceiling: 0.985,
        }
    }
}

/// Converts decibels to linear amplitude multiplier.
#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    if !db.is_finite() {
        return 1.0;
    }
    10.0_f32.powf(db / 20.0)
}

/// Computes ReplayGain linear scale with peak-aware anti-clipping headroom.
pub fn compute_replay_gain_linear(gain_db: f32, peak: f32, enabled: bool) -> f32 {
    if !enabled || !gain_db.is_finite() {
        return 1.0;
    }
    let clamped_db = gain_db.clamp(-24.0, 18.0);
    let mut gain = db_to_linear(clamped_db);
    if peak.is_finite() && peak > 0.001 && gain * peak > 0.98 {
        gain = 0.98 / peak;
    }
    gain
}

/// Equal-power crossfade mixer blending `outgoing` and `incoming` interleaved PCM slices
/// according to normalized progress `t` in `0.0..=1.0`.
pub fn mix_crossfade(outgoing: &[f32], incoming: &[f32], out: &mut [f32], progress: f32) {
    let t = progress.clamp(0.0, 1.0);
    let out_gain = (t * FRAC_PI_2).cos();
    let in_gain = (t * FRAC_PI_2).sin();
    for (i, dst) in out.iter_mut().enumerate() {
        let a = outgoing.get(i).copied().unwrap_or(0.0);
        let b = incoming.get(i).copied().unwrap_or(0.0);
        *dst = a * out_gain + b * in_gain;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FadeDirection {
    None,
    FadeIn,
    FadeOut,
}

/// Smooth S-curve / equal-power fade envelope generator.
#[derive(Debug, Clone)]
pub struct FadeEnvelope {
    direction: FadeDirection,
    total_frames: u64,
    elapsed_frames: u64,
    current_gain: f32,
}

impl Default for FadeEnvelope {
    fn default() -> Self {
        Self::new()
    }
}

impl FadeEnvelope {
    pub fn new() -> Self {
        Self {
            direction: FadeDirection::None,
            total_frames: 0,
            elapsed_frames: 0,
            current_gain: 1.0,
        }
    }

    pub fn trigger_fade_in(&mut self, duration_ms: u32, sample_rate: u32) {
        let frames = ((duration_ms as u64) * (sample_rate as u64)) / 1000;
        if frames == 0 {
            self.direction = FadeDirection::None;
            self.current_gain = 1.0;
            return;
        }
        self.direction = FadeDirection::FadeIn;
        self.total_frames = frames;
        self.elapsed_frames = 0;
        self.current_gain = 0.0;
    }

    pub fn trigger_fade_out(&mut self, duration_ms: u32, sample_rate: u32) {
        let frames = ((duration_ms as u64) * (sample_rate as u64)) / 1000;
        if frames == 0 {
            self.direction = FadeDirection::None;
            self.current_gain = 0.0;
            return;
        }
        self.direction = FadeDirection::FadeOut;
        self.total_frames = frames;
        self.elapsed_frames = 0;
        self.current_gain = 1.0;
    }

    pub fn is_active(&self) -> bool {
        self.direction != FadeDirection::None
    }

    pub fn current_gain(&self) -> f32 {
        self.current_gain
    }

    #[inline]
    pub fn next_frame_gain(&mut self) -> f32 {
        match self.direction {
            FadeDirection::None => self.current_gain,
            FadeDirection::FadeIn => {
                self.elapsed_frames += 1;
                if self.elapsed_frames >= self.total_frames {
                    self.direction = FadeDirection::None;
                    self.current_gain = 1.0;
                } else {
                    let t = self.elapsed_frames as f32 / self.total_frames as f32;
                    self.current_gain = (t * FRAC_PI_2).sin();
                }
                self.current_gain
            }
            FadeDirection::FadeOut => {
                self.elapsed_frames += 1;
                if self.elapsed_frames >= self.total_frames {
                    self.direction = FadeDirection::None;
                    self.current_gain = 0.0;
                } else {
                    let t = self.elapsed_frames as f32 / self.total_frames as f32;
                    self.current_gain = (t * FRAC_PI_2).cos();
                }
                self.current_gain
            }
        }
    }
}

/// Smooth soft-knee headroom limiter preventing harsh digital clipping.
#[derive(Debug, Clone, Copy)]
pub struct SoftLimiter {
    threshold: f32,
    ceiling: f32,
}

impl SoftLimiter {
    pub fn new(threshold: f32, ceiling: f32) -> Self {
        let ceiling = ceiling.clamp(0.5, 0.999);
        let threshold = threshold.clamp(0.1, ceiling - 0.02);
        Self { threshold, ceiling }
    }

    #[inline]
    pub fn process_sample(&self, x: f32) -> f32 {
        if !x.is_finite() {
            return 0.0;
        }
        let abs = x.abs();
        if abs <= self.threshold {
            return x;
        }
        let headroom = self.ceiling - self.threshold;
        let excess = (abs - self.threshold) / headroom;
        let compressed = self.threshold + headroom * excess.tanh();
        compressed.copysign(x)
    }
}

/// Lightweight 16-band real-time spectrum analyzer using a 512-point Hann-windowed
/// radix-2 FFT mapped across logarithmic perceptual frequency bands.
#[derive(Debug, Clone)]
pub struct SpectrumAnalyzer {
    sample_rate: u32,
    ring: [f32; FFT_SIZE],
    write_pos: usize,
    samples_since_fft: usize,
    window: [f32; FFT_SIZE],
    bands: [f32; SPECTRUM_BANDS],
    band_ranges: [(usize, usize); SPECTRUM_BANDS],
}

impl SpectrumAnalyzer {
    pub fn new(sample_rate: u32) -> Self {
        let sample_rate = sample_rate.max(8_000);
        let mut window = [0.0f32; FFT_SIZE];
        for (i, slot) in window.iter_mut().enumerate() {
            let phase = (2.0 * PI * i as f32) / (FFT_SIZE as f32);
            *slot = 0.5 * (1.0 - phase.cos());
        }
        let band_ranges = compute_band_ranges(sample_rate);
        Self {
            sample_rate,
            ring: [0.0; FFT_SIZE],
            write_pos: 0,
            samples_since_fft: 0,
            window,
            bands: [0.0; SPECTRUM_BANDS],
            band_ranges,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let sr = sample_rate.max(8_000);
        if self.sample_rate != sr {
            self.sample_rate = sr;
            self.band_ranges = compute_band_ranges(sr);
        }
    }

    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.write_pos = 0;
        self.samples_since_fft = 0;
        self.bands.fill(0.0);
    }

    pub fn bands(&self) -> [f32; SPECTRUM_BANDS] {
        self.bands
    }

    /// Feeds interleaved PCM samples into the analyzer and updates the 16-band spectrum
    /// whenever a hop of `FFT_SIZE / 2` mono frames completes.
    pub fn push_interleaved(&mut self, samples: &[f32], channels: u16) {
        let ch = (channels as usize).max(1);
        for frame in samples.chunks(ch) {
            let mono = frame.iter().copied().sum::<f32>() / frame.len() as f32;
            self.ring[self.write_pos] = mono;
            self.write_pos = (self.write_pos + 1) % FFT_SIZE;
            self.samples_since_fft += 1;
            if self.samples_since_fft >= FFT_SIZE / 2 {
                self.samples_since_fft = 0;
                self.compute_fft_bands();
            }
        }
    }

    /// Smoothly decays visualizer bars toward zero when paused or stopped.
    pub fn decay(&mut self, factor: f32) {
        let f = factor.clamp(0.0, 0.99);
        for band in &mut self.bands {
            *band *= f;
            if *band < 0.001 {
                *band = 0.0;
            }
        }
    }

    fn compute_fft_bands(&mut self) {
        let mut re = [0.0f32; FFT_SIZE];
        let mut im = [0.0f32; FFT_SIZE];
        for i in 0..FFT_SIZE {
            let idx = (self.write_pos + i) % FFT_SIZE;
            re[i] = self.ring[idx] * self.window[i];
        }
        fft_radix2_in_place(&mut re, &mut im);

        let scale = 2.0 / (FFT_SIZE as f32);
        for (band_idx, &(bin_start, bin_end)) in self.band_ranges.iter().enumerate() {
            let mut sum_sq = 0.0f32;
            let count = (bin_end.saturating_sub(bin_start)).max(1);
            for k in bin_start..bin_end.min(FFT_SIZE / 2) {
                let mag = (re[k] * re[k] + im[k] * im[k]).sqrt() * scale;
                sum_sq += mag * mag;
            }
            let rms = (sum_sq / count as f32).sqrt();
            // Convert RMS magnitude to perceptual normalized 0.0..=1.0 range (-60 dB .. 0 dB)
            let db = 20.0 * (rms + 1e-6).log10();
            let target = ((db + 60.0) / 54.0).clamp(0.0, 1.0);
            let prev = self.bands[band_idx];
            // Fast attack, smooth decay for pleasing visualizer bars
            let alpha = if target > prev { 0.65 } else { 0.25 };
            self.bands[band_idx] = prev + (target - prev) * alpha;
        }
    }
}

fn compute_band_ranges(sample_rate: u32) -> [(usize, usize); SPECTRUM_BANDS] {
    let nyquist = (sample_rate as f32) * 0.5;
    let max_freq = nyquist.clamp(2_000.0, 16_000.0);
    let min_freq = 40.0_f32.min(max_freq * 0.1);
    let ratio = (max_freq / min_freq).powf(1.0 / SPECTRUM_BANDS as f32);
    let bin_hz = sample_rate as f32 / FFT_SIZE as f32;
    let max_bin = FFT_SIZE / 2;

    let mut ranges = [(1usize, 2usize); SPECTRUM_BANDS];
    let mut prev_end = 1usize;
    for (i, slot) in ranges.iter_mut().enumerate() {
        let f_lo = min_freq * ratio.powi(i as i32);
        let f_hi = min_freq * ratio.powi((i + 1) as i32);
        let lo = ((f_lo / bin_hz).floor() as usize).max(prev_end).min(max_bin - 1);
        let hi = ((f_hi / bin_hz).ceil() as usize)
            .max(lo + 1)
            .min(max_bin);
        *slot = (lo, hi);
        prev_end = hi.min(max_bin - 1);
    }
    ranges
}

/// In-place iterative radix-2 Cooley-Tukey FFT (`re.len() == im.len() == power of two`).
fn fft_radix2_in_place(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);

    // Bit-reversal permutation
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while (j & bit) != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    // Butterfly stages
    let mut len = 2usize;
    while len <= n {
        let half = len >> 1;
        let angle = -2.0 * PI / len as f32;
        let w_len_re = angle.cos();
        let w_len_im = angle.sin();
        let mut start = 0usize;
        while start < n {
            let mut w_re = 1.0f32;
            let mut w_im = 0.0f32;
            for k in 0..half {
                let u_idx = start + k;
                let v_idx = u_idx + half;
                let u_re = re[u_idx];
                let u_im = im[u_idx];
                let v_re = re[v_idx] * w_re - im[v_idx] * w_im;
                let v_im = re[v_idx] * w_im + im[v_idx] * w_re;
                re[u_idx] = u_re + v_re;
                im[u_idx] = u_im + v_im;
                re[v_idx] = u_re - v_re;
                im[v_idx] = u_im - v_im;

                let next_w_re = w_re * w_len_re - w_im * w_len_im;
                let next_w_im = w_re * w_len_im + w_im * w_len_re;
                w_re = next_w_re;
                w_im = next_w_im;
            }
            start += len;
        }
        len <<= 1;
    }
}

/// Full real-time DSP chain: click-free volume ramp, ReplayGain dB gain,
/// fade envelope, soft-knee headroom limiter, and 16-band spectrum analyzer.
#[derive(Debug, Clone)]
pub struct DspProcessor {
    config: DspConfig,
    current_gain: f32,
    target_gain: f32,
    ramp_step_per_frame: f32,
    fade: FadeEnvelope,
    limiter: SoftLimiter,
    spectrum: SpectrumAnalyzer,
}

impl DspProcessor {
    pub fn new(config: DspConfig) -> Self {
        let target_gain = Self::compute_target_gain(&config);
        let ramp_frames = ((config.ramp_ms.max(1.0) / 1000.0) * config.sample_rate.max(8_000) as f32)
            .max(1.0);
        Self {
            current_gain: target_gain,
            target_gain,
            ramp_step_per_frame: 1.0 / ramp_frames,
            fade: FadeEnvelope::new(),
            limiter: SoftLimiter::new(config.limiter_threshold, config.limiter_ceiling),
            spectrum: SpectrumAnalyzer::new(config.sample_rate),
            config,
        }
    }

    pub fn configure_stream(&mut self, sample_rate: u32, channels: u16) {
        self.config.sample_rate = sample_rate.max(8_000);
        self.config.channels = channels.max(1);
        let ramp_frames = ((self.config.ramp_ms.max(1.0) / 1000.0)
            * self.config.sample_rate as f32)
            .max(1.0);
        self.ramp_step_per_frame = 1.0 / ramp_frames;
        self.spectrum.set_sample_rate(self.config.sample_rate);
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.config.volume = volume.clamp(0.0, 1.0);
        self.target_gain = Self::compute_target_gain(&self.config);
    }

    pub fn volume(&self) -> f32 {
        self.config.volume
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.config.muted = muted;
        self.target_gain = Self::compute_target_gain(&self.config);
    }

    pub fn is_muted(&self) -> bool {
        self.config.muted
    }

    pub fn set_replay_gain(&mut self, gain_db: f32, peak: f32, enabled: bool) {
        self.config.replay_gain_db = gain_db;
        self.config.replay_gain_peak = peak;
        self.config.replay_gain_enabled = enabled;
        self.target_gain = Self::compute_target_gain(&self.config);
    }

    pub fn trigger_fade_in(&mut self, duration_ms: u32) {
        self.fade
            .trigger_fade_in(duration_ms, self.config.sample_rate);
    }

    pub fn trigger_fade_out(&mut self, duration_ms: u32) {
        self.fade
            .trigger_fade_out(duration_ms, self.config.sample_rate);
    }

    pub fn spectrum(&self) -> [f32; SPECTRUM_BANDS] {
        self.spectrum.bands()
    }

    pub fn decay_spectrum(&mut self, factor: f32) {
        self.spectrum.decay(factor);
    }

    pub fn reset(&mut self) {
        self.current_gain = self.target_gain;
        self.fade = FadeEnvelope::new();
        self.spectrum.reset();
    }

    /// Processes interleaved `f32` PCM samples in-place, updating the 16-band spectrum.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        let ch = (self.config.channels as usize).max(1);
        for frame in samples.chunks_mut(ch) {
            // Smooth linear ramp toward target gain to avoid clicks/zipper noise.
            let diff = self.target_gain - self.current_gain;
            if diff.abs() <= self.ramp_step_per_frame {
                self.current_gain = self.target_gain;
            } else {
                self.current_gain += self.ramp_step_per_frame.copysign(diff);
            }

            let fade_gain = self.fade.next_frame_gain();
            let total_gain = self.current_gain * fade_gain;

            for sample in frame.iter_mut() {
                *sample = self.limiter.process_sample(*sample * total_gain);
            }
        }
        self.spectrum.push_interleaved(samples, self.config.channels);
    }

    fn compute_target_gain(config: &DspConfig) -> f32 {
        if config.muted || config.volume <= 0.0001 {
            return 0.0;
        }
        // Subtle perceptual curve (x^1.5) gives natural loudness response across the slider.
        let vol = config.volume.clamp(0.0, 1.0).powf(1.5);
        let rg = compute_replay_gain_linear(
            config.replay_gain_db,
            config.replay_gain_peak,
            config.replay_gain_enabled,
        );
        vol * rg
    }
}
