use modem::demodulator::Demodulator;
use modem::vox::{SoftwareVox, VoxState};
use num_complex::Complex;
use std::fs::File;
use std::io::{BufReader, Read, Write};

fn write_wav_file(file_path: &str, samples: &[f32]) -> std::io::Result<()> {
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Offline FM Demodulator & FSK Decoder ===");

    let iq_path = "rx_capture.iq";
    let file = match File::open(iq_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "Error: Could not open {}. Did you record a capture first using hackrf_rx_test?",
                iq_path
            );
            return Err(e.into());
        }
    };

    let metadata = file.metadata()?;
    let file_size = metadata.len();
    println!("Reading {} (size: {} bytes)...", iq_path, file_size);

    // === First Pass: Estimate carrier frequency offset during the FSK burst ===
    println!("Running Software PLL frequency estimator over capture...");
    let mut reader_first_pass = BufReader::new(File::open(iq_path)?);
    let mut first_pass_byte_buf = vec![0u8; 65536];

    let mut first_pass_prev = Complex::new(0.0f32, 0.0f32);
    let mut sum_delta = Complex::new(0.0f32, 0.0f32);
    let mut count_delta = 0u64;

    while let Ok(bytes_read) = reader_first_pass.read(&mut first_pass_byte_buf) {
        if bytes_read == 0 {
            break;
        }
        let iq_samples = bytes_read / 2;
        for i in 0..iq_samples {
            let re_i8 = first_pass_byte_buf[2 * i] as i8;
            let im_i8 = first_pass_byte_buf[2 * i + 1] as i8;
            let iq = Complex::new(re_i8 as f32 / 128.0, im_i8 as f32 / 128.0);

            let mag = (iq.re * iq.re + iq.im * iq.im).sqrt();
            if mag > 0.06 {
                // Only average during high-energy FSK burst
                let conj_prev = Complex::new(first_pass_prev.re, -first_pass_prev.im);
                let delta = iq * conj_prev;
                sum_delta += delta;
                count_delta += 1;
            }
            first_pass_prev = iq;
        }
    }

    let offset_radians_per_sample = if count_delta > 0 {
        sum_delta.im.atan2(sum_delta.re)
    } else {
        0.0f32
    };

    let f_offset_hz = offset_radians_per_sample as f64 * 2_400_000.0 / (2.0 * std::f64::consts::PI);
    println!(
        "Estimated SDR frequency offset: {:.1} kHz",
        f_offset_hz / 1000.0
    );

    let mut reader = BufReader::new(file);

    // Each sample is Complex<i8> (2 bytes: 1 byte real, 1 byte imag)
    let total_iq_samples = file_size / 2;
    println!("Total IQ samples in capture: {}", total_iq_samples);

    // IQ Decimation state (by 50: 2.4 MHz -> 48 kHz)
    let mut iq_accumulator = Complex::new(0.0f32, 0.0f32);
    let mut iq_count = 0;

    // FM Demodulator state (at 48 kHz)
    let mut prev_iq_48 = Complex::new(0.0f32, 0.0f32);

    // Frequency mixer (Software PLL) state
    let mut running_phase = 0.0f32;

    // Output audio samples buffer (for WAV export and analysis)
    let mut full_audio = Vec::new();

    // DC Blocker (highpass filter) state to remove tuning offset
    let mut dc_prev_x = 0.0f32;
    let mut dc_prev_y = 0.0f32;
    let dc_alpha = 0.99f32;

    // De-emphasis filter state (75us lowpass filter with alpha = 0.757) to cancel transmitter pre-emphasis
    let mut deemp_prev_y = 0.0f32;
    let deemp_alpha = 0.757f32;

    // Software VOX state
    // Threshold = 0.04, Attack = 10ms, Release = 150ms at 48000 Hz
    let mut vox = SoftwareVox::new(0.04, 10, 150, 48000);

    let mut pre_roll = Vec::new();
    let mut active_buffer = Vec::new();
    let pre_roll_limit = (200 * 48000) / 1000; // 200 ms pre-roll to protect preamble

    let mut decoded_burst_count = 0;

    // Block size for Software VOX (e.g. 1024 samples = 21.3ms)
    let vox_block_size = 1024;
    let mut vox_block = Vec::with_capacity(vox_block_size);

    // Read in chunks of 65,536 bytes (32,768 IQ samples)
    let mut byte_buf = vec![0u8; 65536];

    while let Ok(bytes_read) = reader.read(&mut byte_buf) {
        if bytes_read == 0 {
            break;
        }

        let iq_samples_in_chunk = bytes_read / 2;

        for i in 0..iq_samples_in_chunk {
            // Read real and imag as signed 8-bit integers
            let re_i8 = byte_buf[2 * i] as i8;
            let im_i8 = byte_buf[2 * i + 1] as i8;

            let current_iq = Complex::new(re_i8 as f32 / 128.0, im_i8 as f32 / 128.0);

            // Frequency shift / Mix to center carrier at exactly 0 Hz
            // shifted_iq = current_iq * exp(-j * running_phase)
            let cos_val = (-running_phase).cos();
            let sin_val = (-running_phase).sin();
            let shifted_iq = Complex::new(
                current_iq.re * cos_val - current_iq.im * sin_val,
                current_iq.re * sin_val + current_iq.im * cos_val,
            );
            running_phase += offset_radians_per_sample;

            // 1. Decimate complex IQ by 50 (2.4 MHz -> 48 kHz) to filter out 98% of out-of-band noise
            iq_accumulator += shifted_iq;
            iq_count += 1;

            if iq_count == 50 {
                let iq_48 = iq_accumulator / 50.0;
                iq_accumulator = Complex::new(0.0, 0.0);
                iq_count = 0;

                // 2. FM Demodulate at 48 kHz (pristine narrow-band signal)
                let conj_prev = Complex::new(prev_iq_48.re, -prev_iq_48.im);
                let delta = iq_48 * conj_prev;
                let fm_val_48 = delta.im.atan2(delta.re); // range: [-PI, PI]
                prev_iq_48 = iq_48;

                // Scale the signal so +/- 25 kHz deviation (approx 3.27 radians) is mapped perfectly to +/- 0.98 for unclipped f32 audio
                let mut audio_sample = fm_val_48 * 0.30;

                // 3. Run DC blocker to remove constant frequency offset from tuning
                let cur_x = audio_sample;
                let cur_y = dc_alpha * (dc_prev_y + cur_x - dc_prev_x);
                dc_prev_x = cur_x;
                dc_prev_y = cur_y;
                audio_sample = cur_y;

                // 4. Run De-emphasis filter to restore FSK tone amplitudes to flat response
                let cur_deemp_y =
                    (1.0f32 - deemp_alpha) * audio_sample + deemp_alpha * deemp_prev_y;
                deemp_prev_y = cur_deemp_y;
                audio_sample = cur_deemp_y * 1.5;

                full_audio.push(audio_sample);

                // Accumulate sample into the VOX block
                vox_block.push(audio_sample);

                if vox_block.len() == vox_block_size {
                    // Process the block of 1024 samples through VOX
                    let prev_state = vox.state;
                    let (_rms, _vox_active) = vox.process_block(&vox_block);

                    match (prev_state, vox.state) {
                        (VoxState::Idle, VoxState::Active) => {
                            println!("VOX: Gate Opened! Demodulating burst start...");
                            active_buffer.clear();
                            active_buffer.extend_from_slice(&pre_roll);
                            active_buffer.extend_from_slice(&vox_block);
                        }
                        (VoxState::Active, VoxState::Active) => {
                            active_buffer.extend_from_slice(&vox_block);
                        }
                        (VoxState::Active, VoxState::Idle) => {
                            println!(
                                "VOX: Gate Closed! Gated burst length: {} samples ({:.2}s)",
                                active_buffer.len(),
                                active_buffer.len() as f32 / 48000.0
                            );

                            let demod = Demodulator::new();

                            // Try decoding with FEC
                            let mut frames = demod.demodulate_multi(&active_buffer, true);
                            if !frames.is_empty() {
                                for frame in frames {
                                    if let Ok(payload) = String::from_utf8(frame.payload) {
                                        println!(">>> DECODED FRAME (FEC): {}", payload);
                                        decoded_burst_count += 1;
                                    }
                                }
                            } else {
                                // Try decoding without FEC
                                frames = demod.demodulate_multi(&active_buffer, false);
                                if !frames.is_empty() {
                                    for frame in frames {
                                        if let Ok(payload) = String::from_utf8(frame.payload) {
                                            println!(">>> DECODED FRAME (No FEC): {}", payload);
                                            decoded_burst_count += 1;
                                        }
                                    }
                                } else {
                                    println!(
                                        "VOX: Burst could not be decoded. (Static/noise/corrupted FSK)."
                                    );
                                }
                            }

                            active_buffer.clear();
                            pre_roll.clear();
                        }
                        (VoxState::Idle, VoxState::Idle) => {
                            pre_roll.extend_from_slice(&vox_block);
                            if pre_roll.len() > pre_roll_limit {
                                let drain = pre_roll.len() - pre_roll_limit;
                                pre_roll.drain(0..drain);
                            }
                        }
                    }
                    vox_block.clear();
                }
            }
        }
    }

    // Save demodulated audio for manual inspection/listening
    let wav_path = "demodulated.wav";
    println!("Writing demodulated audio to {}...", wav_path);
    write_wav_file(wav_path, &full_audio)?;

    // Compute stats
    let max_audio = full_audio.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
    let avg_power = full_audio.iter().map(|&x| x * x).sum::<f32>() / full_audio.len() as f32;
    let rms_audio = avg_power.sqrt();

    println!("\n=== Offline Processing Summary ===");
    println!("Max audio amplitude:   {:.4}", max_audio);
    println!("Audio RMS level:       {:.4}", rms_audio);
    println!("Total bursts decoded:  {}", decoded_burst_count);
    println!("WAV audio file written: {}", wav_path);

    if decoded_burst_count > 0 {
        println!("STATUS: Success! Demodulated and decoded frames.");
    } else {
        println!(
            "STATUS: Completed but no valid frames were decoded. Check signal strength, gains, and frequency offset."
        );
    }

    Ok(())
}
