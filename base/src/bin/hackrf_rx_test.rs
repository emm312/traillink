use libhackrf::{DeviceType, HackRf, ffi::SerialNumber};
use num_complex::Complex;
use std::any::Any;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const RX_LNA_GAIN_DB: u32 = 40;
const RX_VGA_GAIN_DB: u32 = 62;
const RX_AMP_ENABLED: bool = true;

struct RxContext {
    to_writer: crossbeam_channel::Sender<Vec<Complex<i8>>>,
    from_writer: crossbeam_channel::Receiver<Vec<Complex<i8>>>,
    to_callback: crossbeam_channel::Sender<Vec<Complex<i8>>>,
    overrun_count: Arc<AtomicUsize>,
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

fn rx_callback_fn(_device: &HackRf, buffer: &[Complex<i8>], user: &dyn Any) {
    if let Some(ctx) = user.downcast_ref::<RxContext>() {
        match ctx.from_writer.try_recv() {
            Ok(mut vec) => {
                vec.clear();
                vec.extend_from_slice(buffer);
                if let Err(crossbeam_channel::TrySendError::Full(vec)) = ctx.to_writer.try_send(vec)
                {
                    // Recycle buffer immediately on overrun
                    let _ = ctx.to_callback.try_send(vec);
                    ctx.overrun_count.fetch_add(1, Ordering::SeqCst);
                }
            }
            Err(_) => {
                // No empty buffers available
                ctx.overrun_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- HackRF RX Test (1.92 MHz, 15 seconds) ---");

    // 1. Open device
    let hackrf = HackRf::open()?;

    // Get board info and handle results safely
    let board_id = hackrf.get_device_type().unwrap_or(DeviceType::Undetected);
    let serial = hackrf.get_serial_number().unwrap_or_default();
    let version = hackrf.version(); // version() returns String directly

    println!("Board ID: {}", device_type_str(&board_id));
    println!("Serial:   {}", format_serial_number(&serial));
    println!("Version:  {}", version);

    // 2. Configure initial settings
    let sample_rate = 2_400_000;
    let freq = 434_000_000; // 434 MHz
    hackrf.set_sample_rate(sample_rate)?;
    hackrf.set_baseband_filter_bandwidth(1_750_000)?;
    hackrf.set_freq(freq)?;

    hackrf.set_amp_enable(RX_AMP_ENABLED)?;
    hackrf.set_lna_gain(RX_LNA_GAIN_DB)?;
    hackrf.set_rxvga_gain(RX_VGA_GAIN_DB)?;

    println!(
        "Configured: sample_rate={} Hz, freq={} Hz",
        sample_rate, freq
    );
    println!(
        "Gains: amp={}, LNA={} dB, RXVGA={} dB",
        if RX_AMP_ENABLED { "ON" } else { "OFF" },
        RX_LNA_GAIN_DB,
        RX_VGA_GAIN_DB
    );

    // Create bounded channels for the ring buffer pattern
    let (to_writer, from_callback) = crossbeam_channel::bounded::<Vec<Complex<i8>>>(100);
    let (to_callback, from_writer) = crossbeam_channel::bounded::<Vec<Complex<i8>>>(100);

    // Pre-allocate buffers and load them into the recycle pool
    // 131072 samples of Complex<i8> = 262144 bytes
    let buffer_size = 131072;
    for _ in 0..100 {
        to_callback.send(Vec::with_capacity(buffer_size)).unwrap();
    }

    let overrun_count = Arc::new(AtomicUsize::new(0));

    // Spawn writer thread
    let to_callback_clone = to_callback.clone();
    let writer_thread = std::thread::spawn(move || {
        let mut file =
            std::fs::File::create("rx_capture.iq").expect("Failed to create rx_capture.iq");
        let mut total_samples = 0;

        let mut sum_sq: f64 = 0.0;
        let mut count: u64 = 0;

        while let Ok(vec) = from_callback.recv() {
            let chunk_len = vec.len();
            total_samples += chunk_len;

            // Write raw bytes
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    vec.as_ptr() as *const u8,
                    chunk_len * std::mem::size_of::<Complex<i8>>(),
                )
            };
            file.write_all(bytes).expect("Failed to write to file");

            // Calculate RMS magnitude contributions
            for sample in &vec {
                let i_val = sample.re as f64;
                let q_val = sample.im as f64;
                let sq_mag = i_val * i_val + q_val * q_val;

                sum_sq += sq_mag;
                count += 1;
            }

            // Recycle buffer
            let _ = to_callback_clone.send(vec);
        }

        let global_rms = if count > 0 {
            (sum_sq / count as f64).sqrt()
        } else {
            0.0
        };

        (total_samples, global_rms)
    });

    // Create the context to pass to the callback
    let rx_ctx = RxContext {
        to_writer,
        from_writer,
        to_callback: to_callback.clone(),
        overrun_count: Arc::clone(&overrun_count),
    };

    // 3. Start RX
    println!("Starting RX stream...");
    let start_time = Instant::now();
    hackrf.start_rx(rx_callback_fn, rx_ctx)?;

    // Stream for 15 seconds with countdown
    let total_secs = 15;
    println!(
        "Capturing to rx_capture.iq for {} seconds. Trigger Pi TX now!",
        total_secs
    );
    for s in (1..=total_secs).rev() {
        println!("... {}s remaining", s);
        std::thread::sleep(Duration::from_secs(1));
    }

    // 4. Stop RX
    println!("Stopping RX stream...");
    hackrf.stop_rx()?;
    let duration = start_time.elapsed();

    // The writer thread will exit when from_callback is disconnected (when rx_ctx inside start_rx is dropped)
    println!("Waiting for writer thread to flush...");
    let (total_samples, global_rms) = writer_thread.join().expect("Writer thread panicked");

    let expected_samples = (duration.as_secs_f64() * sample_rate as f64) as usize;
    let percent_diff =
        ((total_samples as f64 - expected_samples as f64).abs() / expected_samples as f64) * 100.0;
    let overruns = overrun_count.load(Ordering::SeqCst);

    println!("\n--- Results ---");
    println!("Duration:           {:.2} s", duration.as_secs_f64());
    println!("Total samples RX:   {}", total_samples);
    println!("Expected samples:   {}", expected_samples);
    println!("Sample count diff:  {:.3}%", percent_diff);
    println!("Overruns/Drops:     {}", overruns);
    println!("Global RMS:         {:.2}", global_rms);

    if percent_diff < 1.0 && overruns == 0 {
        println!("STATUS: IQ capture successful and saved to rx_capture.iq!");
        Ok(())
    } else {
        Err(format!(
            "Capture issue. Diff={:.3}%, Overruns={}",
            percent_diff, overruns
        )
        .into())
    }
}
