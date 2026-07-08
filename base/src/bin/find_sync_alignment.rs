use std::fs::File;
use std::io::{BufReader, Read};

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open("example.wav")?;
    let mut reader = BufReader::new(file);

    // Skip WAV header (44 bytes)
    let mut header = [0u8; 44];
    reader.read_exact(&mut header)?;

    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;

    let mut samples = Vec::new();
    for chunk in bytes.chunks_exact(2) {
        let val = i16::from_le_bytes([chunk[0], chunk[1]]);
        samples.push(val as f32 / 32767.0);
    }

    // Scan the entire WAV file to find the sync word wherever it starts
    let start_scan = 0;
    let end_scan = samples.len().saturating_sub(16 * 80);

    let tones = [600.0, 1200.0, 1800.0, 2400.0];

    // Expected sync symbols for 0xE14F82C9
    let sync_bytes = [0xE1, 0x4F, 0x82, 0xC9];
    let mut expected = Vec::new();
    for &b in &sync_bytes {
        expected.push(b & 0x03);
        expected.push((b >> 2) & 0x03);
        expected.push((b >> 4) & 0x03);
        expected.push((b >> 6) & 0x03);
    }

    println!("Expected sync symbols: {:?}", expected);
    println!(
        "Scanning sample offsets from {} to {}...",
        start_scan, end_scan
    );

    let mut best_score = 0.0f32;
    let mut best_t = 0;
    let mut best_symbols = Vec::new();

    // Scan every single sample offset to find the absolute absolute peak alignment!
    for t in start_scan..end_scan {
        if t + 16 * 80 > samples.len() {
            break;
        }

        let mut total_score = 0.0;
        let mut decoded_syms = Vec::new();

        for (i, &expected_sym) in expected.iter().enumerate().take(16) {
            let offset = t + i * 80;
            let sym_samples = &samples[offset..offset + 80];

            let mut powers = [0.0; 4];
            let mut sum_powers = 0.0;
            for (s, &tone) in tones.iter().enumerate() {
                let p = goertzel_power(sym_samples, tone);
                powers[s] = p;
                sum_powers += p;
            }

            let mut max_p = -1.0;
            let mut max_tone_idx = 0;
            for (s, &power) in powers.iter().enumerate() {
                if power > max_p {
                    max_p = power;
                    max_tone_idx = s;
                }
            }

            // Map tone index back to symbol via Gray coding:
            // gray = (sym ^ (sym >> 1)) & 0x03
            // Reverse mapping:
            let sym = match max_tone_idx {
                0 => 0, // Gray 0 = Sym 0
                1 => 1, // Gray 1 = Sym 1
                2 => 3, // Gray 2 = Sym 3
                3 => 2, // Gray 3 = Sym 2
                _ => 0,
            };
            decoded_syms.push(sym);

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
            best_t = t;
            best_symbols = decoded_syms;
        }
    }

    println!("\n==================================================");
    println!("   ABSOLUTE PEAK ALIGNMENT FOUND");
    println!("==================================================");
    println!("Peak Offset (Samples): {}", best_t);
    println!("Peak Time (Seconds):   {:.5}s", best_t as f32 / 48000.0);
    println!("Sync Correlation:      {:.4} / 16.0", best_score);
    println!("Expected Symbols:      {:?}", expected);
    println!("Decoded Symbols:       {:?}", best_symbols);

    // Let's print the detailed powers for each symbol at this peak alignment
    println!("\nDetailed Symbol Analysis at Peak Alignment:");
    for i in 0..16 {
        let offset = best_t + i * 80;
        let sym_samples = &samples[offset..offset + 80];
        let mut powers = [0.0; 4];
        for (s, &tone) in tones.iter().enumerate() {
            let p = goertzel_power(sym_samples, tone);
            powers[s] = p;
        }

        let expected_sym = expected[i];
        let expected_tone_idx = ((expected_sym ^ (expected_sym >> 1)) & 0x03) as usize;
        let actual_tone_idx = ((best_symbols[i] ^ (best_symbols[i] >> 1)) & 0x03) as usize;

        println!(
            "Sym {:02} | Expected Sym: {} ({} Hz) | Decoded Sym: {} ({} Hz) | Powers: [{:.1}, {:.1}, {:.1}, {:.1}]",
            i,
            expected_sym,
            tones[expected_tone_idx],
            best_symbols[i],
            tones[actual_tone_idx],
            powers[0] * 10.0,
            powers[1] * 10.0,
            powers[2] * 10.0,
            powers[3] * 10.0
        );
    }

    Ok(())
}
