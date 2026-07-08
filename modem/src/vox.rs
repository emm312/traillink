use rustfft::{FftPlanner, num_complex::Complex};

const DEFAULT_FFT_OPEN_SCORE: f32 = 0.32;
const DEFAULT_FFT_CLOSE_SCORE: f32 = 0.24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoxState {
    Idle,
    Active,
}

pub struct SoftwareVox {
    pub state: VoxState,
    threshold: f32,
    attack_limit_samples: usize,
    release_limit_samples: usize,

    above_threshold_count: usize,
    below_threshold_count: usize,
}

pub struct FskToneSquelch {
    pub state: VoxState,
    min_rms: f32,
    open_score: f32,
    close_score: f32,
    fft_open_score: f32,
    fft_close_score: f32,
    attack_limit_samples: usize,
    release_limit_samples: usize,
    above_threshold_count: usize,
    below_threshold_count: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct FskToneMetrics {
    pub rms: f32,
    pub score: f32,
    pub fft_score: f32,
    pub distinct_tones: usize,
    pub tone_transitions: usize,
}

impl FskToneSquelch {
    pub fn new(
        min_rms: f32,
        open_score: f32,
        close_score: f32,
        attack_ms: u64,
        release_ms: u64,
        sample_rate: usize,
    ) -> Self {
        Self {
            state: VoxState::Idle,
            min_rms,
            open_score,
            close_score,
            fft_open_score: DEFAULT_FFT_OPEN_SCORE,
            fft_close_score: DEFAULT_FFT_CLOSE_SCORE,
            attack_limit_samples: (attack_ms as usize * sample_rate) / 1000,
            release_limit_samples: (release_ms as usize * sample_rate) / 1000,
            above_threshold_count: 0,
            below_threshold_count: 0,
        }
    }

    pub fn update_config(
        &mut self,
        min_rms: f32,
        open_score: f32,
        close_score: f32,
        attack_ms: u64,
        release_ms: u64,
        sample_rate: usize,
    ) {
        self.min_rms = min_rms;
        self.open_score = open_score;
        self.close_score = close_score;
        self.attack_limit_samples = (attack_ms as usize * sample_rate) / 1000;
        self.release_limit_samples = (release_ms as usize * sample_rate) / 1000;
    }

    pub fn process_block(&mut self, samples: &[f32]) -> (FskToneMetrics, bool) {
        let rms = dc_removed_rms(samples);
        let score = fsk_tone_score(samples);
        let fft_score = fsk_fft_score(samples);
        let structure = fsk_symbol_structure(samples);
        let (tone_threshold, fft_threshold) = if self.state == VoxState::Active {
            (self.close_score, self.fft_close_score)
        } else {
            (self.open_score, self.fft_open_score)
        };
        let is_active_block = rms >= self.min_rms
            && score >= tone_threshold
            && fft_score >= fft_threshold
            && structure.distinct_tones >= 2
            && structure.tone_transitions >= 2;

        if is_active_block {
            self.below_threshold_count = 0;
            self.above_threshold_count += samples.len();
            if self.state == VoxState::Idle
                && self.above_threshold_count >= self.attack_limit_samples
            {
                self.state = VoxState::Active;
            }
        } else {
            self.above_threshold_count = 0;
            self.below_threshold_count += samples.len();
            if self.state == VoxState::Active
                && self.below_threshold_count >= self.release_limit_samples
            {
                self.state = VoxState::Idle;
            }
        }

        (
            FskToneMetrics {
                rms,
                score,
                fft_score,
                distinct_tones: structure.distinct_tones,
                tone_transitions: structure.tone_transitions,
            },
            self.state == VoxState::Active,
        )
    }
}

impl SoftwareVox {
    pub fn new(threshold: f32, attack_ms: u64, release_ms: u64, sample_rate: usize) -> Self {
        let attack_limit_samples = (attack_ms as usize * sample_rate) / 1000;
        let release_limit_samples = (release_ms as usize * sample_rate) / 1000;
        Self {
            state: VoxState::Idle,
            threshold,
            attack_limit_samples,
            release_limit_samples,
            above_threshold_count: 0,
            below_threshold_count: 0,
        }
    }

    pub fn update_config(
        &mut self,
        threshold: f32,
        attack_ms: u64,
        release_ms: u64,
        sample_rate: usize,
    ) {
        self.threshold = threshold;
        self.attack_limit_samples = (attack_ms as usize * sample_rate) / 1000;
        self.release_limit_samples = (release_ms as usize * sample_rate) / 1000;
    }

    /// Process a block of samples. Returns (RMS_of_this_block, is_vox_active).
    /// Uses absolute RMS threshold detection to trigger when squelch opens.
    /// Subtracts the mean (DC bias removal) for highly robust RMS calculations.
    pub fn process_block(&mut self, samples: &[f32]) -> (f32, bool) {
        if samples.is_empty() {
            return (0.0, self.state == VoxState::Active);
        }

        // Calculate overall Root-Mean-Square (RMS) for UI metric/telemetry with DC removal
        let rms = dc_removed_rms(samples);

        // Process in symbol-sized chunks (80 samples = 1.67ms) to track envelope
        let chunk_size = 80;
        for chunk in samples.chunks(chunk_size) {
            if chunk.len() < chunk_size {
                continue; // Skip trailing incomplete chunk
            }

            // DC removal per chunk for ultra-robust short-term envelope detection
            let chunk_mean: f32 = chunk.iter().sum::<f32>() / chunk_size as f32;
            let chunk_sum_sq: f32 = chunk
                .iter()
                .map(|&s| {
                    let diff = s - chunk_mean;
                    diff * diff
                })
                .sum();
            let chunk_rms = (chunk_sum_sq / chunk_size as f32).sqrt();

            let mut is_active_chunk = false;

            // Trigger solely based on whether chunk RMS meets or exceeds our threshold,
            // relying on the hardware or software squelch to keep the noise floor quiet.
            if chunk_rms >= self.threshold {
                is_active_chunk = true;
            }

            if is_active_chunk {
                self.below_threshold_count = 0;
                self.above_threshold_count += chunk_size;

                if self.state == VoxState::Idle
                    && self.above_threshold_count >= self.attack_limit_samples
                {
                    self.state = VoxState::Active;
                }
            } else {
                self.above_threshold_count = 0;
                self.below_threshold_count += chunk_size;

                if self.state == VoxState::Active
                    && self.below_threshold_count >= self.release_limit_samples
                {
                    self.state = VoxState::Idle;
                }
            }
        }

        (rms, self.state == VoxState::Active)
    }
}

fn dc_removed_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean: f32 = samples.iter().sum::<f32>() / samples.len() as f32;
    let sum_sq: f32 = samples
        .iter()
        .map(|&sample| {
            let diff = sample - mean;
            diff * diff
        })
        .sum();
    (sum_sq / samples.len() as f32).sqrt()
}

fn fsk_tone_score(samples: &[f32]) -> f32 {
    if samples.len() < crate::SAMPLES_PER_SYMBOL {
        return 0.0;
    }

    (0..crate::SAMPLES_PER_SYMBOL)
        .step_by(8)
        .filter_map(|phase| fsk_tone_score_at_phase(samples, phase))
        .max_by(|a, b| a.total_cmp(b))
        .unwrap_or(0.0)
}

fn fsk_tone_score_at_phase(samples: &[f32], phase: usize) -> Option<f32> {
    if phase + crate::SAMPLES_PER_SYMBOL > samples.len() {
        return None;
    }

    let mut score_sum = 0.0f32;
    let mut symbol_count = 0usize;
    let mut offset = phase;
    while offset + crate::SAMPLES_PER_SYMBOL <= samples.len() {
        let symbol_samples = &samples[offset..offset + crate::SAMPLES_PER_SYMBOL];
        let mut total_power = 0.0f32;
        let mut max_power = 0.0f32;

        for &tone_hz in &crate::TONES {
            let power = goertzel_power(symbol_samples, tone_hz as f64);
            total_power += power;
            max_power = max_power.max(power);
        }

        if total_power > 1e-6 {
            score_sum += max_power / total_power;
            symbol_count += 1;
        }

        offset += crate::SAMPLES_PER_SYMBOL;
    }

    (symbol_count > 0).then(|| score_sum / symbol_count as f32)
}

#[derive(Debug, Clone, Copy)]
struct FskSymbolStructure {
    distinct_tones: usize,
    tone_transitions: usize,
}

fn fsk_symbol_structure(samples: &[f32]) -> FskSymbolStructure {
    (0..crate::SAMPLES_PER_SYMBOL)
        .step_by(8)
        .map(|phase| fsk_symbol_structure_at_phase(samples, phase))
        .max_by(|a, b| {
            a.distinct_tones
                .cmp(&b.distinct_tones)
                .then(a.tone_transitions.cmp(&b.tone_transitions))
        })
        .unwrap_or(FskSymbolStructure {
            distinct_tones: 0,
            tone_transitions: 0,
        })
}

fn fsk_symbol_structure_at_phase(samples: &[f32], phase: usize) -> FskSymbolStructure {
    let mut seen = [false; 4];
    let mut distinct_tones = 0usize;
    let mut tone_transitions = 0usize;
    let mut previous_tone = None;

    let mut offset = phase;
    while offset + crate::SAMPLES_PER_SYMBOL <= samples.len() {
        let chunk = &samples[offset..offset + crate::SAMPLES_PER_SYMBOL];
        let mut best_idx = 0usize;
        let mut best_power = 0.0f32;
        for (idx, &tone_hz) in crate::TONES.iter().enumerate() {
            let power = goertzel_power(chunk, tone_hz as f64);
            if power > best_power {
                best_power = power;
                best_idx = idx;
            }
        }

        if !seen[best_idx] {
            seen[best_idx] = true;
            distinct_tones += 1;
        }
        if previous_tone.is_some_and(|previous| previous != best_idx) {
            tone_transitions += 1;
        }
        previous_tone = Some(best_idx);
        offset += crate::SAMPLES_PER_SYMBOL;
    }

    FskSymbolStructure {
        distinct_tones,
        tone_transitions,
    }
}

fn goertzel_power(samples: &[f32], freq: f64) -> f32 {
    let omega = 2.0 * std::f64::consts::PI * freq / crate::SAMPLE_RATE as f64;
    let coeff = 2.0 * omega.cos();
    let mut s1 = 0.0;
    let mut s2 = 0.0;
    for &x in samples {
        let s0 = x as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2) as f32
}

fn fsk_fft_score(samples: &[f32]) -> f32 {
    let fft_size = samples.len().next_power_of_two().clamp(256, 1024);
    if samples.len() < crate::SAMPLES_PER_SYMBOL || fft_size == 0 {
        return 0.0;
    }

    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let copy_len = samples.len().min(fft_size);
    let mut buffer = vec![Complex::new(0.0f32, 0.0f32); fft_size];
    for (idx, slot) in buffer.iter_mut().enumerate().take(copy_len) {
        let window = if copy_len > 1 {
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * idx as f32 / (copy_len - 1) as f32).cos()
        } else {
            1.0
        };
        slot.re = (samples[idx] - mean) * window;
    }

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);
    fft.process(&mut buffer);

    let max_bin = fft_size / 2;
    let mut total_power = 0.0f32;
    let mut tone_power = 0.0f32;
    for bin in 1..max_bin {
        let freq = bin as f32 * crate::SAMPLE_RATE as f32 / fft_size as f32;
        let power = buffer[bin].norm_sqr();
        total_power += power;
        if crate::TONES
            .iter()
            .any(|&tone| (freq - tone as f32).abs() <= 140.0)
        {
            tone_power += power;
        }
    }

    if total_power <= 1e-9 {
        0.0
    } else {
        (tone_power / total_power).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vox_gate_basic() {
        // Setup VOX with threshold=0.1, attack=10ms (480 samples), release=50ms (2400 samples) at 48kHz
        let mut vox = SoftwareVox::new(0.1, 10, 50, 48000);
        assert_eq!(vox.state, VoxState::Idle);

        // 1. Feed silence (10ms / 480 samples) -> Should stay Idle
        let silence = vec![0.0f32; 480];
        let (rms, active) = vox.process_block(&silence);
        assert!(rms < 0.1);
        assert!(!active);
        assert_eq!(vox.state, VoxState::Idle);

        // 2. Feed a valid 1200 Hz FSK tone (10ms / 480 samples) -> Should attack and transition to Active
        let mut loud = Vec::new();
        let mut phase = 0.0f64;
        for _ in 0..480 {
            loud.push((phase.sin() * 0.5) as f32);
            phase += 2.0 * std::f64::consts::PI * 1200.0 / 48000.0;
        }
        let (rms, active) = vox.process_block(&loud);
        assert!(rms >= 0.1);
        assert!(active);
        assert_eq!(vox.state, VoxState::Active);

        // 3. Feed silence (10ms / 480 samples) -> Should remain Active (release timer is 50ms)
        let (_, active) = vox.process_block(&silence);
        assert!(active);

        // 4. Feed 3 more blocks of silence (30ms total) -> Total silence = 40ms (< 50ms limit). Should remain Active.
        for _ in 0..3 {
            let (_, active) = vox.process_block(&silence);
            assert!(active);
        }

        // 5. Feed 1 more block of silence (10ms) -> Total silence = 50ms (2400 samples). Should trigger Release to Idle.
        let (_, active) = vox.process_block(&silence);
        assert!(!active);
        assert_eq!(vox.state, VoxState::Idle);
    }

    #[test]
    fn test_vox_respects_custom_threshold() {
        // Setup VOX with high threshold=0.4 (amplitude 0.5 tone RMS is ~0.353, which is below 0.4)
        let mut vox = SoftwareVox::new(0.4, 10, 50, 48000);
        assert_eq!(vox.state, VoxState::Idle);

        let mut signal = Vec::new();
        let mut phase = 0.0f64;
        for _ in 0..480 {
            signal.push((phase.sin() * 0.5) as f32); // RMS ≈ 0.353
            phase += 2.0 * std::f64::consts::PI * 1200.0 / 48000.0;
        }

        // Feed tone -> Should stay Idle because RMS (0.353) < threshold (0.4)
        let (rms, active) = vox.process_block(&signal);
        assert!(rms < 0.4);
        assert!(!active);
        assert_eq!(vox.state, VoxState::Idle);

        // Update threshold to 0.3 -> Now it should trigger Active
        vox.update_config(0.3, 10, 50, 48000);
        let (rms, active) = vox.process_block(&signal);
        assert!(rms >= 0.3);
        assert!(active);
        assert_eq!(vox.state, VoxState::Active);
    }

    #[test]
    fn fsk_squelch_rejects_loud_broadband_noise() {
        let mut squelch = FskToneSquelch::new(0.02, 0.62, 0.54, 10, 50, crate::SAMPLE_RATE);
        let mut noise = Vec::new();
        let mut seed = 1u32;
        for _ in 0..960 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let value = ((seed >> 8) as f32 / 16_777_215.0) * 2.0 - 1.0;
            noise.push(value * 0.25);
        }

        let (metrics, active) = squelch.process_block(&noise);
        assert!(metrics.rms > 0.02);
        assert!(
            metrics.fft_score < 0.32,
            "FFT score was {}",
            metrics.fft_score
        );
        assert!(!active, "score was {}", metrics.score);
        assert_eq!(squelch.state, VoxState::Idle);
    }

    #[test]
    fn fsk_squelch_opens_on_modem_tones() {
        let mut squelch = FskToneSquelch::new(0.02, 0.62, 0.54, 10, 50, crate::SAMPLE_RATE);
        let mut signal = Vec::new();
        for i in 0..960 {
            let tone = crate::TONES[(i / crate::SAMPLES_PER_SYMBOL) % crate::TONES.len()] as f64;
            let phase = 2.0 * std::f64::consts::PI * tone * i as f64 / crate::SAMPLE_RATE as f64;
            signal.push((phase.sin() * 0.25) as f32);
        }

        let (metrics, active) = squelch.process_block(&signal);
        assert!(metrics.rms > 0.02);
        assert!(metrics.score > 0.62);
        assert!(
            metrics.fft_score > 0.32,
            "FFT score was {}",
            metrics.fft_score
        );
        assert!(metrics.distinct_tones >= 2);
        assert!(metrics.tone_transitions >= 2);
        assert!(active, "score was {}", metrics.score);
        assert_eq!(squelch.state, VoxState::Active);
    }

    #[test]
    fn fsk_squelch_rejects_single_loud_tone() {
        let mut squelch = FskToneSquelch::new(0.02, 0.62, 0.54, 10, 50, crate::SAMPLE_RATE);
        let mut tone = Vec::new();
        for i in 0..960 {
            let phase = 2.0 * std::f64::consts::PI * 1200.0 * i as f64 / crate::SAMPLE_RATE as f64;
            tone.push((phase.sin() * 0.6) as f32);
        }

        let (metrics, active) = squelch.process_block(&tone);
        assert!(metrics.rms > 0.02);
        assert!(metrics.score > 0.62);
        assert!(metrics.fft_score > 0.32);
        assert_eq!(metrics.distinct_tones, 1);
        assert!(!active);
        assert_eq!(squelch.state, VoxState::Idle);
    }
}
