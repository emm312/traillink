use modem::demodulator::Demodulator;
use std::fs::File;
use std::io::{BufReader, Read};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open("demodulated.wav")?;
    let mut reader = BufReader::new(file);

    let mut header = [0u8; 44];
    reader.read_exact(&mut header)?;

    // Parse RIFF WAV header fields
    let num_channels = u16::from_le_bytes([header[22], header[23]]);
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let bits_per_sample = u16::from_le_bytes([header[34], header[35]]);

    println!("=== WAV file metadata ===");
    println!("Channels:        {}", num_channels);
    println!("Sample Rate:     {} Hz", sample_rate);
    println!("Bits per sample: {}", bits_per_sample);

    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;

    let mut samples = Vec::new();
    if num_channels == 1 {
        for chunk in bytes.chunks_exact(2) {
            let val = i16::from_le_bytes([chunk[0], chunk[1]]);
            samples.push(val as f32 / 32767.0);
        }
    } else if num_channels == 2 {
        for chunk in bytes.chunks_exact(4) {
            let left_val = i16::from_le_bytes([chunk[0], chunk[1]]);
            samples.push(left_val as f32 / 32767.0);
        }
    } else {
        return Err(format!("Unsupported channels count: {}", num_channels).into());
    }

    println!("Read {} mono samples from example.wav", samples.len());

    let demod = Demodulator::new();

    println!("Attempting to decode with FEC...");
    let frames = demod.demodulate_multi(&samples, true);
    if !frames.is_empty() {
        for f in &frames {
            if let Ok(text) = String::from_utf8(f.payload.clone()) {
                println!(">>> DECODED FRAME (FEC): {}", text);
            }
        }
    } else {
        println!("No frames decoded with FEC.");
    }

    println!("\nAttempting to decode without FEC...");
    let frames_no_fec = demod.demodulate_multi(&samples, false);
    if !frames_no_fec.is_empty() {
        for f in &frames_no_fec {
            if let Ok(text) = String::from_utf8(f.payload.clone()) {
                println!(">>> DECODED FRAME (No FEC): {}", text);
            }
        }
    } else {
        println!("No frames decoded without FEC.");
    }

    Ok(())
}
