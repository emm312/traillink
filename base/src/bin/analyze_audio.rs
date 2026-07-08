use std::fs::File;
use std::io::{BufReader, Read};

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

    println!("Read {} audio samples from demodulated.wav", samples.len());

    // 100ms block size = 4800 samples
    let block_size = 4800;
    let mut max_block_rms = 0.0f32;
    let mut max_block_idx = 0;

    println!("\n--- RMS Profile (100ms blocks) ---");
    for (idx, block) in samples.chunks(block_size).enumerate() {
        let sum_sq: f32 = block.iter().map(|&x| x * x).sum();
        let rms = (sum_sq / block.len() as f32).sqrt();

        // Print blocks with non-zero energy to see the structure
        if rms > 0.002 {
            println!(
                "Block {:03}: Time {:.1}s - {:.1}s | RMS = {:.6} | Peak = {:.6}",
                idx,
                (idx * block_size) as f32 / 48000.0,
                ((idx + 1) * block_size) as f32 / 48000.0,
                rms,
                block.iter().map(|x| x.abs()).fold(0.0f32, f32::max)
            );
        }

        if rms > max_block_rms {
            max_block_rms = rms;
            max_block_idx = idx;
        }
    }

    println!(
        "\nMax block RMS: {:.6} at block {} (Time: {:.1}s)",
        max_block_rms,
        max_block_idx,
        (max_block_idx * block_size) as f32 / 48000.0
    );

    Ok(())
}
