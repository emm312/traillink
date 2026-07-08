use std::fs::File;
use std::io::{BufReader, Read};

fn goertzel_power(samples: &[f32], freq: f64, sample_rate: f64) -> f32 {
    let omega = 2.0 * std::f64::consts::PI * freq / sample_rate;
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
    let file = File::open("demodulated.wav")?;
    let mut reader = BufReader::new(file);

    // Skip WAV header
    let mut header = [0u8; 44];
    reader.read_exact(&mut header)?;

    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;

    let mut samples = Vec::new();
    for chunk in bytes.chunks_exact(2) {
        let val = i16::from_le_bytes([chunk[0], chunk[1]]);
        samples.push(val as f32 / 32767.0);
    }

    // Noise interval (0.0 to 2.0s)
    let noise_start = (0.0 * 48000.0) as usize;
    let noise_end = (2.0 * 48000.0) as usize;
    let mut noise_samples = samples[noise_start..noise_end].to_vec();

    // Scale noise to simulate loud open-squelch static (RMS ≈ 0.25)
    // First, find current RMS of noise
    let sum_sq: f32 = noise_samples.iter().map(|&x| x * x).sum();
    let current_rms = (sum_sq / noise_samples.len() as f32).sqrt();
    println!("Original Noise RMS: {:.6}", current_rms);

    let target_rms = 0.25f32;
    let scale = target_rms / current_rms;
    for x in &mut noise_samples {
        *x *= scale;
    }
    let scaled_sum_sq: f32 = noise_samples.iter().map(|&x| x * x).sum();
    println!(
        "Scaled Noise RMS:   {:.6}",
        (scaled_sum_sq / noise_samples.len() as f32).sqrt()
    );

    // FSK burst interval (3.8s to 4.5s)
    let burst_start = (3.8 * 48000.0) as usize;
    let burst_end = (4.5 * 48000.0) as usize;
    let burst_samples = &samples[burst_start..burst_end];

    let tones = [600.0, 1200.0, 1800.0, 2400.0];

    // Analyze noise (scaled loud)
    let mut noise_ratios = Vec::new();
    let mut noise_peakiness = Vec::new();
    for chunk in noise_samples.chunks_exact(80) {
        let chunk_sum_sq: f32 = chunk.iter().map(|&s| s * s).sum();
        let mut powers = [0.0; 4];
        let mut max_power = 0.0f32;
        for (i, &tone) in tones.iter().enumerate() {
            let p = goertzel_power(chunk, tone, 48000.0);
            powers[i] = p;
            if p > max_power {
                max_power = p;
            }
        }
        let sum_powers: f32 = powers.iter().sum();

        let ratio = max_power / (chunk_sum_sq * 40.0 + 1e-5);
        let peakiness = max_power / (sum_powers + 1e-5);
        noise_ratios.push(ratio);
        noise_peakiness.push(peakiness);
    }

    // Analyze burst
    let mut burst_ratios = Vec::new();
    let mut burst_peakiness = Vec::new();
    for chunk in burst_samples.chunks_exact(80) {
        let chunk_sum_sq: f32 = chunk.iter().map(|&s| s * s).sum();
        let mut powers = [0.0; 4];
        let mut max_power = 0.0f32;
        for (i, &tone) in tones.iter().enumerate() {
            let p = goertzel_power(chunk, tone, 48000.0);
            powers[i] = p;
            if p > max_power {
                max_power = p;
            }
        }
        let sum_powers: f32 = powers.iter().sum();

        let ratio = max_power / (chunk_sum_sq * 40.0 + 1e-5);
        let peakiness = max_power / (sum_powers + 1e-5);
        burst_ratios.push(ratio);
        burst_peakiness.push(peakiness);
    }

    fn stats(v: &[f32], thresh: f32) -> (f32, f32, f32, f32) {
        let min = v.iter().fold(f32::MAX, |a, &b| a.min(b));
        let max = v.iter().fold(f32::MIN, |a, &b| a.max(b));
        let avg = v.iter().sum::<f32>() / v.len() as f32;
        let count_thresh = v.iter().filter(|&&x| x >= thresh).count() as f32 / v.len() as f32;
        (min, max, avg, count_thresh)
    }

    let (nr_min, nr_max, nr_avg, nr_028) = stats(&noise_ratios, 0.28);
    let (br_min, br_max, br_avg, br_028) = stats(&burst_ratios, 0.28);

    println!("\n=== Simulated LOUD Noise vs Burst ===");
    println!("--- Tonal Ratio (Thresh 0.28) ---");
    println!(
        "Noise: Min={:.3}, Max={:.3}, Avg={:.3}, Frac>=0.28={:.1}%",
        nr_min,
        nr_max,
        nr_avg,
        nr_028 * 100.0
    );
    println!(
        "Burst: Min={:.3}, Max={:.3}, Avg={:.3}, Frac>=0.28={:.1}%",
        br_min,
        br_max,
        br_avg,
        br_028 * 100.0
    );

    let (nr_min_35, nr_max_35, nr_avg_35, nr_035) = stats(&noise_ratios, 0.35);
    let (br_min_35, br_max_35, br_avg_35, br_035) = stats(&burst_ratios, 0.35);
    println!("--- Tonal Ratio (Thresh 0.35) ---");
    println!(
        "Noise: Min={:.3}, Max={:.3}, Avg={:.3}, Frac>=0.35={:.1}%",
        nr_min_35,
        nr_max_35,
        nr_avg_35,
        nr_035 * 100.0
    );
    println!(
        "Burst: Min={:.3}, Max={:.3}, Avg={:.3}, Frac>=0.35={:.1}%",
        br_min_35,
        br_max_35,
        br_avg_35,
        br_035 * 100.0
    );

    let (nr_min_40, nr_max_40, nr_avg_40, nr_040) = stats(&noise_ratios, 0.40);
    let (br_min_40, br_max_40, br_avg_40, br_040) = stats(&burst_ratios, 0.40);
    println!("--- Tonal Ratio (Thresh 0.40) ---");
    println!(
        "Noise: Min={:.3}, Max={:.3}, Avg={:.3}, Frac>=0.40={:.1}%",
        nr_min_40,
        nr_max_40,
        nr_avg_40,
        nr_040 * 100.0
    );
    println!(
        "Burst: Min={:.3}, Max={:.3}, Avg={:.3}, Frac>=0.40={:.1}%",
        br_min_40,
        br_max_40,
        br_avg_40,
        br_040 * 100.0
    );

    let (nr_min_45, nr_max_45, nr_avg_45, nr_045) = stats(&noise_ratios, 0.45);
    let (br_min_45, br_max_45, br_avg_45, br_045) = stats(&burst_ratios, 0.45);
    println!("--- Tonal Ratio (Thresh 0.45) ---");
    println!(
        "Noise: Min={:.3}, Max={:.3}, Avg={:.3}, Frac>=0.45={:.1}%",
        nr_min_45,
        nr_max_45,
        nr_avg_45,
        nr_045 * 100.0
    );
    println!(
        "Burst: Min={:.3}, Max={:.3}, Avg={:.3}, Frac>=0.45={:.1}%",
        br_min_45,
        br_max_45,
        br_avg_45,
        br_045 * 100.0
    );

    let (np_min, np_max, np_avg, np_075) = stats(&noise_peakiness, 0.75);
    let (bp_min, bp_max, bp_avg, bp_075) = stats(&burst_peakiness, 0.75);
    println!("\n--- Spectral Peakiness (Thresh 0.75) ---");
    println!(
        "Noise: Min={:.3}, Max={:.3}, Avg={:.3}, Frac>=0.75={:.1}%",
        np_min,
        np_max,
        np_avg,
        np_075 * 100.0
    );
    println!(
        "Burst: Min={:.3}, Max={:.3}, Avg={:.3}, Frac>=0.75={:.1}%",
        bp_min,
        bp_max,
        bp_avg,
        bp_075 * 100.0
    );

    Ok(())
}
