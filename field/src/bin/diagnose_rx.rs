use modem::demodulator::Demodulator;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|chunk| {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            sample as f32 / i16::MAX as f32
        })
        .collect()
}

fn write_wav_header(writer: &mut BufWriter<File>, num_samples: usize) -> std::io::Result<()> {
    let data_size = num_samples * 2;
    let riff_size = 36 + data_size;

    // RIFF header
    writer.write_all(b"RIFF")?;
    writer.write_all(&(riff_size as u32).to_le_bytes())?;
    writer.write_all(b"WAVE")?;

    // fmt chunk
    writer.write_all(b"fmt ")?;
    writer.write_all(&(16u32).to_le_bytes())?; // Chunk size
    writer.write_all(&(1u16).to_le_bytes())?; // Audio format (1 = PCM)
    writer.write_all(&(1u16).to_le_bytes())?; // Num channels (1 = mono)
    writer.write_all(&(48000u32).to_le_bytes())?; // Sample rate
    writer.write_all(&(96000u32).to_le_bytes())?; // Byte rate
    writer.write_all(&(2u16).to_le_bytes())?; // Block align
    writer.write_all(&(16u16).to_le_bytes())?; // Bits per sample

    // data chunk
    writer.write_all(b"data")?;
    writer.write_all(&(data_size as u32).to_le_bytes())?;

    Ok(())
}

fn goertzel_power(samples: &[f32], freq: f64) -> f32 {
    let omega = 2.0 * std::f64::consts::PI * freq / 48000.0;
    let coeff = 2.0 * omega.cos();
    let mut s1 = 0.0;
    let mut s2 = 0.0;
    for &x in samples {
        let s0 = (x as f64) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2) as f32
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("==================================================");
    println!("       TrailLink God-Level Receiver Diagnosis     ");
    println!("==================================================");

    let device =
        std::env::var("FIELD_DEVICE").unwrap_or_else(|_| "plughw:CARD=Elite,DEV=0".to_string());

    println!("Targeting Audio Input Device: {}", device);
    println!("Spawning arecord capture (S16_LE, 48000 Hz, Mono, Squelch Disabled)...");
    println!("Recording for 5.0 seconds. Press transmit on the HackRF now!");

    let mut child = Command::new("arecord")
        .args([
            "-D", &device, "-f", "S16_LE", "-r", "48000", "-t", "raw", "-c", "1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let mut stdout = child.stdout.take().ok_or("Failed to open arecord stdout")?;

    // 5 seconds * 48000 samples/sec * 2 bytes/sample = 480,000 bytes
    let total_bytes_to_read = 480000;
    let mut raw_bytes = Vec::with_capacity(total_bytes_to_read);
    let mut temp_buf = vec![0u8; 4096];

    let start_time = std::time::Instant::now();
    while raw_bytes.len() < total_bytes_to_read {
        let elapsed = start_time.elapsed();
        if elapsed > Duration::from_secs(6) {
            println!("Timeout waiting for audio samples!");
            break;
        }

        if let Ok(n) = stdout.read(&mut temp_buf).await {
            if n == 0 {
                break; // EOF
            }
            raw_bytes.extend_from_slice(&temp_buf[..n]);
        }
    }

    // Stop arecord
    let _ = child.kill().await;

    let num_samples = raw_bytes.len() / 2;
    println!("Capture Finished! Collected {} samples.", num_samples);

    if num_samples == 0 {
        eprintln!(
            "ERROR: Captured 0 bytes. Your audio card configuration is completely silent or invalid."
        );
        return Ok(());
    }

    // Convert to f32 samples
    let samples = bytes_to_f32(&raw_bytes[..num_samples * 2]);

    // Save to WAV file
    let wav_path = "diagnose_rx.wav";
    println!("Writing captured audio to {}...", wav_path);
    let file = File::create(wav_path)?;
    let mut writer = BufWriter::new(file);
    write_wav_header(&mut writer, num_samples)?;
    for chunk in raw_bytes.chunks_exact(2) {
        writer.write_all(chunk)?;
    }
    writer.flush()?;
    println!("WAV file saved successfully. You can download and listen to it!");

    // --- DSP & Amplitude Analysis ---
    let sum_sq: f32 = samples.iter().map(|&x| x * x).sum();
    let avg_rms = (sum_sq / num_samples as f32).sqrt();
    let peak_amp = samples.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);

    // Count clipped samples (magnitude >= 0.99)
    let clip_count = samples.iter().filter(|&&x| x.abs() >= 0.99).count();
    let clip_percentage = (clip_count as f64 / num_samples as f64) * 100.0;

    println!("\n--- Amplitude Diagnostics ---");
    println!("Average RMS Level:   {:.6}", avg_rms);
    println!("Peak Amplitude:      {:.6}", peak_amp);
    println!(
        "Clipped Samples:     {} ({:.3}%)",
        clip_count, clip_percentage
    );

    if avg_rms < 0.002 {
        println!("WARNING: Extremely low volume! Check if card is muted or microphone gain is 0.");
    } else if clip_percentage > 2.0 {
        println!(
            "WARNING: Heavy clipping detected ({:.1}% of samples are saturated)! The radio speaker volume is way too high. TURN IT DOWN to prevent FSK distortion.",
            clip_percentage
        );
    } else {
        println!("Volume Level status: OK.");
    }

    // --- Sync Correlation Diagnostics ---
    println!("\n--- Sync Word Correlation Scan ---");
    println!("Scanning 5.0 seconds of audio for sync word...");

    let demod = Demodulator::new();

    // Expected sync symbols
    let sync_bytes = [
        (modem::SYNC_WORD >> 24) as u8,
        (modem::SYNC_WORD >> 16) as u8,
        (modem::SYNC_WORD >> 8) as u8,
        modem::SYNC_WORD as u8,
    ];
    let mut expected_sync_symbols = Vec::with_capacity(16);
    for &b in &sync_bytes {
        expected_sync_symbols.extend_from_slice(&modem::modulator::Modulator::byte_to_symbols(b));
    }

    let mut best_score = -1.0f32;
    let mut best_idx = 0;

    // Scan step size 4 for high-resolution peak detection
    for t in (0..samples.len().saturating_sub(16 * 80)).step_by(4) {
        // compute sync score
        let mut total_score = 0.0;
        for (i, &expected_sym) in expected_symbols_iter(&expected_sync_symbols).enumerate() {
            let offset = t + i * 80;
            let sym_samples = &samples[offset..offset + 80];

            let mut powers = [0.0; 4];
            let mut sum_powers = 0.0;
            for (s, &tone_hz) in modem::TONES.iter().enumerate() {
                let p = goertzel_power(sym_samples, tone_hz as f64);
                powers[s] = p;
                sum_powers += p;
            }

            if sum_powers > 1e-6 {
                let expected_tone_idx = ((expected_sym ^ (expected_sym >> 1)) & 0x03) as usize;
                let expected_power = powers[expected_tone_idx];
                total_score += expected_power / sum_powers;
            } else {
                total_score += 0.25;
            }
        }

        if total_score > best_score {
            best_score = total_score;
            best_idx = t;
        }
    }

    println!(
        "Best Sync Score Found: {:.2} / 16.0 (at Time: {:.2}s)",
        best_score,
        best_idx as f32 / 48000.0
    );
    println!(" - Coarse Threshold: 9.50");
    println!(" - Fine Threshold:   10.50");

    if best_score >= 9.5 {
        println!("STATUS: Coarse Sync detected successfully!");
        if best_score >= 10.50 {
            println!("STATUS: Perfect Fine Sync detected! Frame should decode perfectly.");
        } else {
            println!(
                "WARNING: Sync correlation is marginal (between 9.5 and 10.5). The signal is likely too noisy or distorted."
            );
        }
    } else {
        println!("ERROR: Sync word NOT found. The correlation score is too low.");
        println!("Possible reasons:");
        println!(" 1. The transmitter carrier is on but no data is playing.");
        println!(
            " 2. Frequency deviation mismatch (ensure transmitter is on 12.5 kHz FM deviation)."
        );
        println!(" 3. Heavy audio distortion/overdrive.");
    }

    // Spectral analysis of the neighborhood around the best sync window
    if best_score > 3.0 {
        println!("\n--- Spectral Analysis around Peak Sync Window ---");
        let sym_samples = &samples[best_idx..best_idx + 80];
        let mut powers = [0.0; 4];
        let mut sum_powers = 0.0;
        for (s, &tone_hz) in modem::TONES.iter().enumerate() {
            let p = goertzel_power(sym_samples, tone_hz as f64);
            powers[s] = p;
            sum_powers += p;
        }

        println!("Symbol 0 Goertzel Tone Powers:");
        for (i, &power) in powers.iter().enumerate() {
            let norm_p = if sum_powers > 0.0 {
                power / sum_powers
            } else {
                0.0
            };
            println!(
                " - Tone {} ({} Hz): Power = {:.6} ({:.1}%)",
                i,
                modem::TONES[i],
                powers[i],
                norm_p * 100.0
            );
        }
    }

    // --- Demodulator attempt ---
    println!("\n--- Full Demodulator Execution ---");
    let frames = demod.demodulate_multi(&samples, true);
    if !frames.is_empty() {
        println!("SUCCESS! Demodulated {} FEC-encoded frames:", frames.len());
        for (i, frame) in frames.iter().enumerate() {
            if let Ok(payload) = String::from_utf8(frame.payload.clone()) {
                println!(" [{}] Payload: \"{}\"", i, payload);
            }
        }
    } else {
        let frames_no_fec = demod.demodulate_multi(&samples, false);
        if !frames_no_fec.is_empty() {
            println!(
                "SUCCESS! Demodulated {} non-FEC frames:",
                frames_no_fec.len()
            );
            for (i, frame) in frames_no_fec.iter().enumerate() {
                if let Ok(payload) = String::from_utf8(frame.payload.clone()) {
                    println!(" [{}] Payload: \"{}\"", i, payload);
                }
            }
        } else {
            println!("FAIL: Demodulator output is completely empty.");
        }
    }

    Ok(())
}

fn expected_symbols_iter(expected_sync_symbols: &[u8]) -> std::slice::Iter<'_, u8> {
    expected_sync_symbols.iter()
}
