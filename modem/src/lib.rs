pub mod demodulator;
pub mod fec;
pub mod frame;
pub mod image;
pub mod location;
pub mod modulator;
pub mod vox;

pub const SAMPLE_RATE: usize = 48000;
pub const SYMBOL_RATE: usize = 600;
pub const SAMPLES_PER_SYMBOL: usize = SAMPLE_RATE / SYMBOL_RATE; // 80

pub const TONES: [usize; 4] = [600, 1200, 1800, 2400];

// Robust 32-bit sync word
pub const SYNC_WORD: u32 = 0xE14F82C9;

pub const FRAME_HEADER_BYTES: usize = 3;
pub const CRC_BYTES: usize = 2;
pub const CRC_BLOCK_PAYLOAD_BYTES: usize = 255;
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 2048;
pub const IMAGE_CHUNK_DATA_BYTES: usize = 250;
pub const IMAGE_CHUNK_HEADER_BYTES: usize = 8;
pub const MAX_IMAGE_BYTES: usize = 256 * 1024;

#[cfg(test)]
mod integration_tests {
    use super::*;
    use demodulator::Demodulator;
    use frame::{Frame, MsgType};
    use modulator::Modulator;

    // A simple, self-contained pseudo-random number generator for tests
    fn next_random(state: &mut u32) -> f64 {
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        (*state as f64) / (u32::MAX as f64)
    }

    // Helper to generate Gaussian noise using Box-Muller transform
    fn add_gaussian_noise(samples: &mut [f32], snr_db: f32, seed: u32) {
        let mut rng_state = seed;
        let mut signal_power = 0.0;
        for &x in samples.iter() {
            signal_power += (x * x) as f64;
        }
        signal_power /= samples.len() as f64;

        let snr_linear = 10.0f64.powf((snr_db as f64) / 10.0);
        let noise_power = signal_power / snr_linear;
        let noise_std_dev = noise_power.sqrt();

        let mut i = 0;
        while i < samples.len() {
            let u1 = next_random(&mut rng_state);
            let u2 = next_random(&mut rng_state);

            let r = (-2.0 * u1.ln()).sqrt() * noise_std_dev;
            let theta = 2.0 * std::f64::consts::PI * u2;

            let z0 = r * theta.cos();
            let z1 = r * theta.sin();

            samples[i] += z0 as f32;
            if i + 1 < samples.len() {
                samples[i + 1] += z1 as f32;
            }
            i += 2;
        }
    }

    #[test]
    fn test_clean_round_trip_no_fec() {
        let frame = Frame::new(1, MsgType::Response, false, b"Hello World!".to_vec()).unwrap();

        let mut modulator = Modulator::new();
        let samples = modulator.modulate(&frame, false, 100);

        let demodulator = Demodulator::new();
        let decoded = demodulator.demodulate(&samples, false).unwrap();

        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_clean_round_trip_with_fec() {
        let frame = Frame::new(2, MsgType::SOS, true, b"Help near VK2EMM!".to_vec()).unwrap();

        let mut modulator = Modulator::new();
        let samples = modulator.modulate(&frame, true, 100);

        let demodulator = Demodulator::new();
        let decoded = demodulator.demodulate(&samples, true).unwrap();

        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_round_trip_with_offset() {
        let frame = Frame::new(0, MsgType::Query, false, b"Test offset".to_vec()).unwrap();

        let mut modulator = Modulator::new();
        let mut modulated_samples = modulator.modulate(&frame, false, 100);

        // Prepend 123 samples of silence/zero (not aligned to symbol boundaries)
        let mut samples = vec![0.0f32; 123];
        samples.append(&mut modulated_samples);
        // Append 100 samples of silence/zero
        samples.extend(vec![0.0f32; 100]);

        let demodulator = Demodulator::new();
        let decoded = demodulator.demodulate(&samples, false).unwrap();

        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_round_trip_with_noise_no_fec() {
        let frame = Frame::new(
            1,
            MsgType::Broadcast,
            false,
            b"Broadcasting with some noise...".to_vec(),
        )
        .unwrap();

        let mut modulator = Modulator::new();
        let mut samples = modulator.modulate(&frame, false, 100);

        // Add 15 dB SNR noise
        add_gaussian_noise(&mut samples, 15.0, 42);

        let demodulator = Demodulator::new();
        let decoded = demodulator.demodulate(&samples, false).unwrap();

        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_round_trip_with_noise_and_fec() {
        let frame = Frame::new(
            1,
            MsgType::Broadcast,
            false,
            b"Broadcasting with high noise...".to_vec(),
        )
        .unwrap();

        let mut modulator = Modulator::new();
        let mut samples = modulator.modulate(&frame, true, 100);

        // Add high noise: 10 dB SNR (lower SNR than no FEC)
        add_gaussian_noise(&mut samples, 10.0, 1337);

        let demodulator = Demodulator::new();
        let decoded = demodulator.demodulate(&samples, true).unwrap();

        assert_eq!(frame, decoded);
    }

    // Helper to resample a signal to simulate sample-rate clock skew in PPM
    fn resample_skew(samples: &[f32], ppm: f64) -> Vec<f32> {
        let factor = 1.0 + (ppm / 1_000_000.0);
        let mut resampled = Vec::new();
        let mut i = 0;
        loop {
            let t = (i as f64) / factor;
            let t_floor = t.floor() as usize;
            let t_ceil = t_floor + 1;
            if t_ceil >= samples.len() {
                break;
            }
            let frac = t - (t_floor as f64);
            let s = (1.0 - frac) * (samples[t_floor] as f64) + frac * (samples[t_ceil] as f64);
            resampled.push(s as f32);
            i += 1;
        }
        resampled
    }

    #[test]
    fn test_round_trip_with_sample_rate_skew() {
        // Build a medium-length frame to allow skew drift to accumulate
        let frame = Frame::new(
            1,
            MsgType::Query,
            true,
            b"Testing clock skew drift over longer payload lengths...".to_vec(),
        )
        .unwrap();

        let mut modulator = Modulator::new();
        let samples = modulator.modulate(&frame, true, 100);

        // 1. Test positive skew: +200 ppm (receiver's clock is slower)
        let skewed_plus = resample_skew(&samples, 200.0);
        let demodulator = Demodulator::new();
        let decoded_plus = demodulator.demodulate(&skewed_plus, true).unwrap();
        assert_eq!(frame, decoded_plus, "Failed on +200 ppm skew");

        // 2. Test negative skew: -200 ppm (receiver's clock is faster)
        let skewed_minus = resample_skew(&samples, -200.0);
        let decoded_minus = demodulator.demodulate(&skewed_minus, true).unwrap();
        assert_eq!(frame, decoded_minus, "Failed on -200 ppm skew");
    }

    // Helper to simulate analog FM radio channel passband distortion and FM limiter soft-clipping
    fn apply_radio_channel_effects(samples: &mut [f32]) {
        // Cascade first-order highpass filter at 300 Hz
        let mut prev_x = 0.0;
        let mut prev_y = 0.0;
        let alpha_hp = 0.9622;
        for x in samples.iter_mut() {
            let cur_x = *x as f64;
            let y = alpha_hp * (prev_y + cur_x - prev_x);
            prev_x = cur_x;
            prev_y = y;
            *x = y as f32;
        }

        // Cascade first-order lowpass filter at 3000 Hz
        let mut prev_y = 0.0;
        let alpha_lp = 0.2819;
        for x in samples.iter_mut() {
            let cur_x = *x as f64;
            let y = prev_y + alpha_lp * (cur_x - prev_y);
            prev_y = y;
            *x = y as f32;
        }

        // Apply soft clipping (saturation) with a gain drive
        let gain = 1.5f32;
        for x in samples.iter_mut() {
            let val = *x * gain;
            *x = val.tanh();
        }
    }

    #[test]
    fn test_round_trip_with_channel_realism() {
        let frame = Frame::new(
            1,
            MsgType::Response,
            true,
            b"VK2EMM: Signal is 59 with light QRM.".to_vec(),
        )
        .unwrap();

        let mut modulator = Modulator::new();
        let mut samples = modulator.modulate(&frame, true, 100);

        // Apply bandpass filter (300-3000 Hz) and soft-clipping saturation
        apply_radio_channel_effects(&mut samples);

        // Add 12 dB SNR noise (moderate channel noise)
        add_gaussian_noise(&mut samples, 12.0, 9876);

        let demodulator = Demodulator::new();
        let decoded = demodulator.demodulate(&samples, true).unwrap();

        assert_eq!(frame, decoded);
    }

    // WAV file exporter helper
    fn write_wav_file(file_path: &str, samples: &[f32]) -> std::io::Result<()> {
        use std::fs::File;
        use std::io::Write;

        let mut file = File::create(file_path)?;
        let num_samples = samples.len();
        let data_size = num_samples * 2;
        let riff_size = 36 + data_size;

        // RIFF header
        file.write_all(b"RIFF")?;
        file.write_all(&(riff_size as u32).to_le_bytes())?;
        file.write_all(b"WAVE")?;

        // fmt chunk
        file.write_all(b"fmt ")?;
        file.write_all(&(16u32).to_le_bytes())?; // Chunk size
        file.write_all(&(1u16).to_le_bytes())?; // Audio format (1 = PCM)
        file.write_all(&(1u16).to_le_bytes())?; // Num channels (1 = mono)
        file.write_all(&(48000u32).to_le_bytes())?; // Sample rate
        file.write_all(&(96000u32).to_le_bytes())?; // Byte rate
        file.write_all(&(2u16).to_le_bytes())?; // Block align
        file.write_all(&(16u16).to_le_bytes())?; // Bits per sample

        // data chunk
        file.write_all(b"data")?;
        file.write_all(&(data_size as u32).to_le_bytes())?;

        // Convert f32 samples to 16-bit signed integers
        for &s in samples {
            let clamped = s.clamp(-1.0, 1.0);
            let sample_i16 = (clamped * 32767.0) as i16;
            file.write_all(&sample_i16.to_le_bytes())?;
        }

        Ok(())
    }

    #[test]
    fn test_export_wav_demo() {
        // This test simulates real-world hardware by using a 500 ms preamble,
        // and exports the modulated audio to "modem_demo.wav" for listening.
        let frame = Frame::new(
            1,
            MsgType::Query,
            true,
            b"VK2EMM calling base station. Do you copy? GPS Lat: -33.8688, Lon: 151.2093".to_vec(),
        )
        .unwrap();

        let mut modulator = Modulator::new();
        let samples = modulator.modulate(&frame, true, 500); // 500 ms preamble for hardware settling

        // Export to WAV
        write_wav_file("modem_demo.wav", &samples).unwrap();

        // Ensure the export can also be successfully demodulated by our software demodulator
        let demodulator = Demodulator::new();
        let decoded = demodulator.demodulate(&samples, true).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_round_trip_with_noisy_pre_padding() {
        // Build a frame
        let frame = Frame::new(
            1,
            MsgType::Response,
            false,
            b"Under leading static squelch noise!".to_vec(),
        )
        .unwrap();

        let mut modulator = Modulator::new();
        let modulated_samples = modulator.modulate(&frame, true, 100);

        // Create 2000 samples of pure leading Gaussian static noise (~12.5 symbols worth)
        let mut leading_noise = vec![0.0f32; 2000];
        add_gaussian_noise(&mut leading_noise, -3.0, 54321); // very high noise (0.5 RMS amplitude)

        // Combine leading noise, the modulated signal, and some trailing noise
        let mut samples = leading_noise;
        samples.extend_from_slice(&modulated_samples);

        let mut trailing_noise = vec![0.0f32; 1000];
        add_gaussian_noise(&mut trailing_noise, -3.0, 9999);
        samples.extend_from_slice(&trailing_noise);

        // Run demodulation
        let demodulator = Demodulator::new();
        let decoded = demodulator.demodulate(&samples, true).unwrap();

        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_round_trip_with_slow_vox_truncation() {
        // Build a frame
        let frame = Frame::new(
            1,
            MsgType::Response,
            true,
            b"Slow VOX chopped my preamble, but I still copy!".to_vec(),
        )
        .unwrap();

        let mut modulator = Modulator::new();
        // Modulate with a 500 ms preamble (300 symbols = 24,000 samples)
        let modulated_samples = modulator.modulate(&frame, true, 500);

        // Simulate a slow VOX key-up by chopping off the first 400 ms (19,200 samples) of the audio.
        // This leaves only 100 ms (4,800 samples) of the preamble, but the sync word and payload are fully intact.
        let chop_samples = (400 * 48000) / 1000; // 19,200 samples
        let truncated_samples = modulated_samples[chop_samples..].to_vec();

        // Run demodulation
        let demodulator = Demodulator::new();
        let decoded = demodulator.demodulate(&truncated_samples, true).unwrap();

        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_export_wav_multi_demo() {
        // This test builds 5 distinct frames, modulates them, concatenates them
        // with static noise/silence gaps in between, and writes them to "modem_multi_demo.wav".
        let frames = [
            Frame::new(
                1,
                MsgType::Query,
                true,
                b"VK2EMM calling base station. [Frame 1/5]".to_vec(),
            )
            .unwrap(),
            Frame::new(
                1,
                MsgType::Response,
                true,
                b"Base station copying loud and clear! [Frame 2/5]".to_vec(),
            )
            .unwrap(),
            Frame::new(
                1,
                MsgType::Query,
                true,
                b"Sending GPS report: Lat -33.8688, Lon 151.2093. [Frame 3/5]".to_vec(),
            )
            .unwrap(),
            Frame::new(
                1,
                MsgType::Response,
                true,
                b"Copy that! Dispatching search and rescue teams. [Frame 4/5]".to_vec(),
            )
            .unwrap(),
            Frame::new(
                1,
                MsgType::Ack,
                false,
                b"Roger! Over and out. [Frame 5/5]".to_vec(),
            )
            .unwrap(),
        ];

        let mut modulator = Modulator::new();
        let mut continuous_samples = Vec::new();

        // Initial 100 ms leading silence/noise
        continuous_samples.extend(vec![0.0f32; 4800]);

        for (idx, frame) in frames.iter().enumerate() {
            // Modulate with standard 500 ms hardware-resilient preamble
            let modulated = modulator.modulate(frame, true, 500);
            continuous_samples.extend_from_slice(&modulated);

            // Add some random noise & silence gap (say 300 ms) in between transmissions
            let mut gap = vec![0.0f32; 14400]; // 300 ms gap
            add_gaussian_noise(&mut gap, 12.0, idx as u32 * 100 + 42); // moderate static noise
            continuous_samples.extend_from_slice(&gap);
        }

        // Write the concatenated stream to WAV file
        write_wav_file("modem_multi_demo.wav", &continuous_samples).unwrap();

        // Run multi-frame demodulator to verify we decode all 5 frames perfectly
        let demodulator = Demodulator::new();
        let decoded_frames = demodulator.demodulate_multi(&continuous_samples, true);

        assert_eq!(decoded_frames.len(), 5);
        for i in 0..5 {
            assert_eq!(frames[i], decoded_frames[i], "Frame {} mismatch", i + 1);
        }
    }

    #[test]
    fn test_round_trip_with_full_analog_loopback_simulation() {
        // Build a medium-length frame
        let frame = Frame::new(
            1,
            MsgType::Query,
            true,
            b"VK2EMM: testing robust timing synchronization (ts) skew!".to_vec(),
        )
        .unwrap();

        let mut modulator = Modulator::new();
        let modulated_samples = modulator.modulate(&frame, true, 100);

        // 1. Simulate 150 ppm clock mismatch (soundcard clock mismatch / timing skew)
        let mut samples = resample_skew(&modulated_samples, 150.0);

        // 2. Simulate DC Offset (+0.04)
        for x in samples.iter_mut() {
            *x += 0.04;
        }

        // 3. Simulate Unknown Gain (attenuation / path loss, e.g. scale by 0.3)
        for x in samples.iter_mut() {
            *x *= 0.3;
        }

        // 4. Simulate Radio Passband Distortion (300-3000 Hz)
        apply_radio_channel_effects(&mut samples);

        // 5. Add Room Squelch/Static Noise (moderate room noise: 12 dB SNR)
        add_gaussian_noise(&mut samples, 12.0, 777);

        // Run demodulation
        let demodulator = Demodulator::new();
        let decoded = demodulator.demodulate(&samples, true).unwrap();

        assert_eq!(frame, decoded);
    }
}
