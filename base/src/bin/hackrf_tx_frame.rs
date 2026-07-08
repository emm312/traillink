use libhackrf::{DeviceType, HackRf, ffi::SerialNumber};
use modem::frame::{Frame, MsgType};
use modem::modulator::Modulator;
use num_complex::Complex;
use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct TxContext {
    samples: Vec<Complex<i8>>,
    index: AtomicUsize,
    signaled: AtomicBool,
    done_sender: crossbeam_channel::Sender<()>,
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
        let samples_len = ctx.samples.len();

        for sample in samples.iter_mut() {
            if idx < samples_len {
                *sample = ctx.samples[idx];
                idx += 1;
            } else {
                // Once the precomputed buffer is exhausted, transmit silence (zero IQ)
                *sample = Complex::new(0, 0);
            }
        }
        ctx.index.store(idx, Ordering::Relaxed);

        if idx >= samples_len && !ctx.signaled.swap(true, Ordering::SeqCst) {
            let _ = ctx.done_sender.try_send(());
        }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- HackRF Frame TX to Quansheng ---");
    println!("WARNING: TX SAFETY CONSTRAINTS ENFORCED.");
    println!(" - Frequency: 434.000 MHz");
    println!(" - TX VGA Gain: 47 dB (MAXIMUM POWER)");
    println!(" - Amp: ON (14 dB)");
    println!(" - Ant: None / Dummy load only");

    // 1. Generate the modem Frame
    let payload =
        b"Hello Quansheng from HackRF! This is a test packet with 7kHz FM deviation.".to_vec();
    let frame = Frame::new(1, MsgType::Broadcast, true, payload)?;
    println!("Generated Frame: {:?}", frame);

    // 2. Modulate frame into 48 kHz AFSK audio
    let mut modulator = Modulator::new();
    // 500 ms preamble to allow hardware receiver AGC and squelch to settle
    let audio_samples = modulator.modulate(&frame, true, 500);
    println!(
        "Generated {} AFSK audio samples at 48 kHz",
        audio_samples.len()
    );

    // 3. Prepend and append 500ms of silence (unmodulated carrier)
    // 48000 samples/sec * 0.5s = 24000 samples
    let silence_len = 24000;
    let mut full_audio = Vec::with_capacity(audio_samples.len() + 2 * silence_len);
    full_audio.resize(silence_len, 0.0f32);
    full_audio.extend_from_slice(&audio_samples);
    full_audio.resize(full_audio.len() + silence_len, 0.0f32);

    // 3.5 Apply Pre-emphasis filter to restore high-frequency flat response at receiver
    // y[n] = x[n] - alpha * x[n-1]
    let alpha = 0.98;
    let mut pre_emphasized = Vec::with_capacity(full_audio.len());
    let mut prev_x = 0.0f32;
    for &x in &full_audio {
        let y = x - alpha * prev_x;
        prev_x = x;
        pre_emphasized.push(y);
    }
    // Normalize to maintain maximum peak at 1.0 (keeping deviation exact)
    let max_peak = pre_emphasized
        .iter()
        .map(|x| x.abs())
        .fold(0.0f32, f32::max);
    if max_peak > 0.0 {
        for val in pre_emphasized.iter_mut() {
            *val /= max_peak;
        }
    }
    let audio_source = pre_emphasized;

    // 4. FM Modulate and Upsample to 2 Msps
    let sample_rate_rf = 2_000_000.0;
    let deviation_hz = 7000.0; // 7 kHz deviation (user requested)
    let mut tx_samples =
        Vec::with_capacity(((audio_source.len() as f64) * sample_rate_rf / 48000.0) as usize);

    let mut phase = 0.0f64;
    for n in 0.. {
        let t_audio = (n as f64) * 48000.0 / sample_rate_rf;
        let idx_audio = t_audio as usize;
        if idx_audio >= audio_source.len() {
            break;
        }

        // Linear interpolation of the baseband audio samples
        let frac = t_audio - idx_audio as f64;
        let s0 = audio_source[idx_audio] as f64;
        let s1 = if idx_audio + 1 < audio_source.len() {
            audio_source[idx_audio + 1] as f64
        } else {
            s0
        };
        let interp_audio = s0 + frac * (s1 - s0);

        // Update phase
        // phase += 2 * pi * f_dev * audio_sample / F_rf
        phase += 2.0 * std::f64::consts::PI * deviation_hz * interp_audio / sample_rate_rf;
        if phase >= 2.0 * std::f64::consts::PI {
            phase -= 2.0 * std::f64::consts::PI;
        } else if phase < 0.0 {
            phase += 2.0 * std::f64::consts::PI;
        }

        // Convert to Complex<i8> with max amplitude 125 (max is 127) for 1.9 dB stronger signal
        let re = (phase.cos() * 125.0).round() as i8;
        let im = (phase.sin() * 125.0).round() as i8;
        tx_samples.push(Complex::new(re, im));
    }

    let duration_secs = tx_samples.len() as f64 / sample_rate_rf;
    println!(
        "Precomputed {} RF IQ samples (Duration: {:.2}s)",
        tx_samples.len(),
        duration_secs
    );

    // 5. Open HackRF device
    let hackrf = HackRf::open()?;

    let board_id = hackrf.get_device_type().unwrap_or(DeviceType::Undetected);
    let serial = hackrf.get_serial_number().unwrap_or_default();
    let version = hackrf.version();

    println!("Board ID: {}", device_type_str(&board_id));
    println!("Serial:   {}", format_serial_number(&serial));
    println!("Version:  {}", version);

    // 6. Configure settings
    let freq = 434_000_000;
    hackrf.set_sample_rate(sample_rate_rf as u32)?;
    hackrf.set_freq(freq)?;
    hackrf.set_txvga_gain(47)?; // 47 dB (Maximum TX VGA Gain)
    hackrf.set_amp_enable(true)?; // Amp enabled (MAXIMUM POWER)

    println!(
        "Configured: sample_rate={} Hz, freq={} Hz, tx_vga=47 dB (MAX), amp=ON",
        sample_rate_rf, freq
    );

    let (done_sender, done_receiver) = crossbeam_channel::bounded::<()>(1);

    let tx_ctx = TxContext {
        samples: tx_samples,
        index: AtomicUsize::new(0),
        signaled: AtomicBool::new(false),
        done_sender,
    };

    // 7. Start TX
    println!("Starting TX stream...");
    let start_time = Instant::now();
    hackrf.start_tx(tx_callback_fn, tx_ctx)?;

    // Wait until the callback signals that all samples have been transmitted
    let timeout_duration = Duration::from_secs_f64(duration_secs + 2.0);
    if done_receiver.recv_timeout(timeout_duration).is_err() {
        println!("Warning: Timeout waiting for transmission to complete!");
    }

    // 8. Stop TX
    println!("Stopping TX stream...");
    hackrf.stop_tx()?;
    let duration = start_time.elapsed();

    println!("\n--- Results ---");
    println!("Actual TX Duration: {:.2} s", duration.as_secs_f64());
    println!("STATUS: Frame transmitted successfully!");

    Ok(())
}
