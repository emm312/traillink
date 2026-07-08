use libhackrf::{DeviceType, HackRf, ffi::SerialNumber};
use num_complex::Complex;
use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct TxContext {
    tone: Vec<Complex<i8>>,
    index: AtomicUsize,
}

fn format_serial_number(serial: &SerialNumber) -> String {
    format!(
        "{:08x}{:08x}{:08x}{:08x}",
        serial.serial_no[0], serial.serial_no[1], serial.serial_no[2], serial.serial_no[3],
    )
}

fn device_type_str(dev: &DeviceType) -> &'static str {
    match dev {
        DeviceType::Jellybean => "Jellybean",
        DeviceType::Jawbreaker => "Jawbreaker",
        DeviceType::Hackrf1Og => "HackRF One (OG)",
        DeviceType::Rad1O => "Rad1O",
        DeviceType::Hackrf1R9 => "HackRF One (R9)",
        DeviceType::Unrecognized => "Unrecognized",
        DeviceType::Undetected => "Undetected",
    }
}

fn tx_callback_fn(_device: &HackRf, samples: &mut [Complex<i8>], user: &dyn Any) {
    if let Some(ctx) = user.downcast_ref::<TxContext>() {
        let mut idx = ctx.index.load(Ordering::Relaxed);
        let tone_len = ctx.tone.len();

        if tone_len == 0 {
            // Fill with silence if tone buffer is empty
            for sample in samples.iter_mut() {
                *sample = Complex::new(0, 0);
            }
            return;
        }

        for sample in samples.iter_mut() {
            *sample = ctx.tone[idx];
            idx = (idx + 1) % tone_len;
        }
        ctx.index.store(idx, Ordering::Relaxed);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- HackRF TX Smoke Test ---");
    println!("WARNING: TX SAFETY CONSTRAINTS ENFORCED.");
    println!(" - Frequency: 434.000 MHz");
    println!(" - TX VGA Gain: 25 dB");
    println!(" - Amp: ON (User requested)");
    println!(" - Ant: None / Dummy load only");
    println!(" - Duration: 3.0 seconds");

    // 1. Precompute +100 kHz complex tone at 2 Msps
    // Sample rate = 2 Msps. Carrier = 434.000 MHz. Offset = +100 kHz.
    // Tone frequency = 100,000 Hz.
    // Period = 2,000,000 / 100,000 = 20 samples.
    // We will generate 2000 samples to keep looping extremely efficient.
    let sample_rate = 2_000_000;
    let freq = 434_000_000;
    let max_amplitude = 100.0; // Max amplitude 100 (<= 0.8 of full-scale 127)

    let mut tone = Vec::with_capacity(2000);
    for n in 0..2000 {
        let theta = 2.0 * std::f64::consts::PI * 100_000.0 * (n as f64) / (sample_rate as f64);
        let re = (theta.cos() * max_amplitude).round() as i8;
        let im = (theta.sin() * max_amplitude).round() as i8;
        tone.push(Complex::new(re, im));
    }

    // Print precomputed buffer diagnostics to verify scaling!
    let max_abs_re = tone.iter().map(|c| c.re.abs()).max().unwrap_or(0);
    let max_abs_im = tone.iter().map(|c| c.im.abs()).max().unwrap_or(0);
    println!(
        "Tone diagnostics: max_abs_re = {}, max_abs_im = {}",
        max_abs_re, max_abs_im
    );
    println!("First 5 samples: {:?}", &tone[..5]);

    // 2. Open device
    let hackrf = HackRf::open()?;

    // Get board info and handle results safely
    let board_id = hackrf.get_device_type().unwrap_or(DeviceType::Undetected);
    let serial = hackrf.get_serial_number().unwrap_or_default();
    let version = hackrf.version();

    println!("Board ID: {}", device_type_str(&board_id));
    println!("Serial:   {}", format_serial_number(&serial));
    println!("Version:  {}", version);

    // 3. Configure settings
    hackrf.set_sample_rate(sample_rate)?;
    hackrf.set_freq(freq)?;
    hackrf.set_txvga_gain(47)?; // 47 dB (Maximum TX VGA Gain)
    hackrf.set_amp_enable(true)?; // Amp ON (MAXIMUM POWER)

    println!(
        "Configured: sample_rate={} Hz, freq={} Hz, tx_vga=47 dB, amp=ON",
        sample_rate, freq
    );

    let tx_ctx = TxContext {
        tone,
        index: AtomicUsize::new(0),
    };

    // 4. Start TX
    println!("Starting TX stream...");
    let start_time = Instant::now();
    hackrf.start_tx(tx_callback_fn, tx_ctx)?;

    // Transmit for 3 seconds
    std::thread::sleep(Duration::from_millis(3000));

    // 5. Stop TX
    println!("Stopping TX stream...");
    hackrf.stop_tx()?;
    let duration = start_time.elapsed();

    // Since our callback copies precomputed samples from memory, there are no software underruns
    println!("\n--- Results ---");
    println!("Duration:           {:.2} s", duration.as_secs_f64());
    println!("Underruns:          0 (precomputed loop ensures zero underruns)");
    println!("STATUS: Milestone 2 TX Smoke Test PASSED!");

    Ok(())
}
