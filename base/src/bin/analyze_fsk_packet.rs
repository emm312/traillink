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

    println!("Loaded {} samples from example.wav", samples.len());

    // FSK burst starts around 2.3s (sample index: 2.3 * 48000 = 110,400)
    // and ends around 3.9s (sample index: 3.9 * 48000 = 187,200)
    let start_sample = 110400;
    let end_sample = 187200;

    if samples.len() < end_sample {
        println!("Error: File too short!");
        return Ok(());
    }

    println!("\n--- FSK Burst Symbol Analysis (Symbols of 80 samples) ---");
    println!("Analyzing symbols in the burst region (2.3s to 3.9s)...");

    let tones = [600.0, 1200.0, 1800.0, 2400.0];

    // We analyze symbols 280 to 350 to see the end of the 500ms preamble (300 symbols) and the sync word!
    for sym_idx in 280..350 {
        let offset = start_sample + sym_idx * 80;
        if offset + 80 > samples.len() {
            break;
        }

        let sym_samples = &samples[offset..offset + 80];
        let mut powers = [0.0; 4];
        let mut sum_powers = 0.0;
        for (i, &tone) in tones.iter().enumerate() {
            let p = goertzel_power(sym_samples, tone);
            powers[i] = p;
            sum_powers += p;
        }

        let mut max_power = 0.0;
        let mut max_tone_idx = 0;
        for (i, &power) in powers.iter().enumerate() {
            if power > max_power {
                max_power = power;
                max_tone_idx = i;
            }
        }

        let norm_max = if sum_powers > 0.0 {
            max_power / sum_powers
        } else {
            0.0
        };

        // Print details of each symbol
        let time_sec = offset as f32 / 48000.0;
        println!(
            "Sym {:03} | Time: {:.3}s | Max Tone: {} ({} Hz) | Conf: {:.1}% | Powers: [{:.1}, {:.1}, {:.1}, {:.1}]",
            sym_idx,
            time_sec,
            max_tone_idx,
            tones[max_tone_idx],
            norm_max * 100.0,
            powers[0] * 10.0,
            powers[1] * 10.0,
            powers[2] * 10.0,
            powers[3] * 10.0
        );
    }

    Ok(())
}
