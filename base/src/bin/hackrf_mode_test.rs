use libhackrf::{DeviceType, HackRf};
use num_complex::Complex;
use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const RX_LNA_GAIN_DB: u32 = 40;
const RX_VGA_GAIN_DB: u32 = 62;
const RX_AMP_ENABLED: bool = true;

struct ModeContext {
    tone: Vec<Complex<i8>>,
    index: AtomicUsize,
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

fn rx_callback_fn(_device: &HackRf, _samples: &[Complex<i8>], _user: &dyn Any) {
    // Just discard samples for this mode transition test
}

fn tx_callback_fn(_device: &HackRf, samples: &mut [Complex<i8>], user: &dyn Any) {
    if let Some(ctx) = user.downcast_ref::<ModeContext>() {
        let mut idx = ctx.index.load(Ordering::Relaxed);
        let tone_len = ctx.tone.len();
        if tone_len == 0 {
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

// Configures device for RX
fn setup_rx(hackrf: &HackRf) -> Result<(), Box<dyn std::error::Error>> {
    hackrf.set_sample_rate(2_000_000)?;
    hackrf.set_freq(434_000_000)?;
    hackrf.set_amp_enable(RX_AMP_ENABLED)?;
    hackrf.set_lna_gain(RX_LNA_GAIN_DB)?;
    hackrf.set_rxvga_gain(RX_VGA_GAIN_DB)?;
    Ok(())
}

// Configures device for TX
fn setup_tx(hackrf: &HackRf) -> Result<(), Box<dyn std::error::Error>> {
    hackrf.set_sample_rate(2_000_000)?;
    hackrf.set_freq(434_000_000)?;
    hackrf.set_txvga_gain(47)?;
    hackrf.set_amp_enable(true)?;
    Ok(())
}

fn run_sequence(
    mut hackrf: HackRf,
    context: &ModeContext,
    try_direct_transitions: bool,
) -> Result<(HackRf, bool), Box<dyn std::error::Error>> {
    let mut needed_reopen = false;

    println!("\n--- Step 1: RX (2 seconds) ---");
    setup_rx(&hackrf)?;
    hackrf.start_rx(rx_callback_fn, ())?;
    std::thread::sleep(Duration::from_millis(2000));
    hackrf.stop_rx()?;
    println!("Step 1 RX stopped cleanly.");

    println!("\n--- Step 2: TX (1 second) ---");
    let mut tx_started = false;

    if try_direct_transitions {
        println!("Attempting direct RX -> TX transition...");
        if let Err(e) = setup_tx(&hackrf).and_then(|_| {
            hackrf
                .start_tx(tx_callback_fn, ())
                .map_err(|err| err.into())
        }) {
            println!("Direct RX -> TX transition FAILED: {:?}.", e);
            println!("Applying workaround: closing and reopening device...");
            needed_reopen = true;
        } else {
            println!("Direct RX -> TX transition SUCCEEDED!");
            tx_started = true;
        }
    }

    if !tx_started {
        // Workaround / Reopen path
        drop(hackrf);
        std::thread::sleep(Duration::from_millis(500)); // Cool down
        hackrf = HackRf::open()?;
        setup_tx(&hackrf)?;

        // Construct context clone
        let step_ctx = ModeContext {
            tone: context.tone.clone(),
            index: AtomicUsize::new(0),
        };
        hackrf.start_tx(tx_callback_fn, step_ctx)?;
        needed_reopen = true;
    }

    std::thread::sleep(Duration::from_millis(1000));
    hackrf.stop_tx()?;
    println!("Step 2 TX stopped cleanly.");

    println!("\n--- Step 3: RX (2 seconds) ---");
    let mut rx_started = false;

    if try_direct_transitions && !needed_reopen {
        println!("Attempting direct TX -> RX transition...");
        if let Err(e) = setup_rx(&hackrf).and_then(|_| {
            hackrf
                .start_rx(rx_callback_fn, ())
                .map_err(|err| err.into())
        }) {
            println!("Direct TX -> RX transition FAILED: {:?}.", e);
            println!("Applying workaround: closing and reopening device...");
            needed_reopen = true;
        } else {
            println!("Direct TX -> RX transition SUCCEEDED!");
            rx_started = true;
        }
    }

    if !rx_started {
        // Workaround / Reopen path
        drop(hackrf);
        std::thread::sleep(Duration::from_millis(500)); // Cool down
        hackrf = HackRf::open()?;
        setup_rx(&hackrf)?;
        hackrf.start_rx(rx_callback_fn, ())?;
        needed_reopen = true;
    }

    std::thread::sleep(Duration::from_millis(2000));
    hackrf.stop_rx()?;
    println!("Step 3 RX stopped cleanly.");

    Ok((hackrf, needed_reopen))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- HackRF Mode Transition Test ---");

    // Precompute a small tone for the TX step
    let mut tone = Vec::with_capacity(200);
    for n in 0..200 {
        let theta = 2.0 * std::f64::consts::PI * 100_000.0 * (n as f64) / 2_000_000.0;
        let re = (theta.cos() * 100.0).round() as i8;
        let im = (theta.sin() * 100.0).round() as i8;
        tone.push(Complex::new(re, im));
    }

    let context = ModeContext {
        tone,
        index: AtomicUsize::new(0),
    };

    // Open initial device
    let hackrf = HackRf::open()?;
    let board_id = hackrf.get_device_type().unwrap_or(DeviceType::Undetected);
    println!("Opened board: {}", device_type_str(&board_id));

    // Sequence 1
    println!("\n========== Sequence 1 (Direct Transition Test) ==========");
    let (hackrf, needed_reopen) = run_sequence(hackrf, &context, true)?;

    if needed_reopen {
        println!("\nRESULT: Mode transitions required the close-and-reopen workaround.");
    } else {
        println!("\nRESULT: Direct transitions completed successfully without close-and-reopen!");
    }

    // Sequence 2 (to verify reliability twice in a row)
    println!("\n========== Sequence 2 (Reliability Verification) ==========");
    // We pass !needed_reopen to try_direct_transitions to skip testing direct again if we already know it fails
    let (_hackrf, second_needed_reopen) = run_sequence(hackrf, &context, !needed_reopen)?;

    println!("\n--- Reliability Report ---");
    println!("Sequence 1 Reopen Needed: {}", needed_reopen);
    println!("Sequence 2 Reopen Needed: {}", second_needed_reopen);
    println!("STATUS: Milestone 3 Mode Transitions Test PASSED!");

    Ok(())
}
