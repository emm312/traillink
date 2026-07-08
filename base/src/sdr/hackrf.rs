use crossbeam_channel::{Receiver, Sender};
use libhackrf::HackRf;
use modem::demodulator::Demodulator;
use modem::frame::{Frame, MsgType};
use modem::image::parse_image_chunk_payload;
use modem::modulator::Modulator;
use modem::vox::{FskToneSquelch, VoxState};
use num_complex::Complex;
use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const RX_LNA_GAIN_DB: u32 = 40;
const RX_VGA_GAIN_DB: u32 = 62;
const RX_AMP_ENABLED: bool = true;
const FSK_SQUELCH_MIN_RMS: f32 = 0.015;
const FSK_SQUELCH_OPEN_SCORE: f32 = 0.62;
const FSK_SQUELCH_CLOSE_SCORE: f32 = 0.54;
const FSK_SQUELCH_ATTACK_MS: u64 = 10;
const FSK_SQUELCH_RELEASE_MS: u64 = 150;

#[derive(Clone)]
struct TransceiverRxContext {
    to_processor: Sender<Vec<Complex<i8>>>,
    recycle_rx: Receiver<Vec<Complex<i8>>>,
    to_recycle_pool: Sender<Vec<Complex<i8>>>,
    overrun_count: Arc<AtomicUsize>,
}

fn rx_callback_fn(_device: &HackRf, buffer: &[Complex<i8>], user: &dyn Any) {
    if let Some(ctx) = user.downcast_ref::<TransceiverRxContext>() {
        match ctx.recycle_rx.try_recv() {
            Ok(mut vec) => {
                vec.clear();
                vec.extend_from_slice(buffer);
                if let Err(crossbeam_channel::TrySendError::Full(vec)) =
                    ctx.to_processor.try_send(vec)
                {
                    let _ = ctx.to_recycle_pool.try_send(vec);
                    ctx.overrun_count.fetch_add(1, Ordering::SeqCst);
                }
            }
            Err(_) => {
                ctx.overrun_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

struct TransceiverTxContext {
    samples: Vec<Complex<i8>>,
    index: AtomicUsize,
    signaled: AtomicBool,
    done_sender: Sender<()>,
}

fn tx_callback_fn(_device: &HackRf, samples: &mut [Complex<i8>], user: &dyn Any) {
    if let Some(ctx) = user.downcast_ref::<TransceiverTxContext>() {
        let mut idx = ctx.index.load(Ordering::Relaxed);
        let samples_len = ctx.samples.len();

        for sample in samples.iter_mut() {
            if idx < samples_len {
                *sample = ctx.samples[idx];
                idx += 1;
            } else {
                *sample = Complex::new(0, 0);
            }
        }
        ctx.index.store(idx, Ordering::Relaxed);

        if idx >= samples_len && !ctx.signaled.swap(true, Ordering::SeqCst) {
            let _ = ctx.done_sender.try_send(());
        }
    }
}

pub enum SdrCommand {
    Transmit(String),
    TransmitFrame {
        msg_type: MsgType,
        has_location: bool,
        payload: String,
    },
}

#[derive(Clone, Debug)]
pub struct LinkTelemetry {
    pub snr_db: Option<f32>,
    pub rssi_dbm: Option<f32>,
    pub rssi_percent: f32,
    pub fec_corrections: usize,
    pub crc_pass: Option<bool>,
    pub decoded_frames: usize,
    pub failed_frames: usize,
    pub sync_score: Option<f32>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum SdrEvent {
    Frame {
        msg_type: MsgType,
        has_location: bool,
        payload: String,
    },
    Telemetry(LinkTelemetry),
    Notice(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioDirection {
    Rx,
    Tx,
}

pub struct AudioBlock {
    pub samples: Vec<f32>,
    pub direction: AudioDirection,
}

fn estimate_rssi(samples: &[f32]) -> (f32, Option<f32>) {
    if samples.is_empty() {
        return (0.0, None);
    }
    let sum_sq: f32 = samples.iter().map(|sample| sample * sample).sum();
    let rms = (sum_sq / samples.len() as f32).sqrt();
    let percent = ((rms / 0.5) * 100.0).clamp(0.0, 100.0);
    let dbfs = 20.0 * rms.max(1e-6).log10();
    // Relative estimate: calibrated enough for trend display, not lab-grade dBm.
    let dbm = (dbfs - 70.0).clamp(-130.0, -30.0);
    (percent, Some(dbm))
}

fn image_output_path(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "out.webp"
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "out.png"
    } else {
        "out.jpeg"
    }
}

fn spawn_tx_audio_preview(audio_tx: Sender<AudioBlock>, samples: Vec<f32>) {
    std::thread::spawn(move || {
        let block_interval = Duration::from_secs_f64(1024.0 / 48_000.0);
        for chunk in samples.chunks_exact(1024) {
            if audio_tx
                .send(AudioBlock {
                    samples: chunk.to_vec(),
                    direction: AudioDirection::Tx,
                })
                .is_err()
            {
                break;
            }
            std::thread::sleep(block_interval);
        }
    });
}

pub fn run_transceiver_loop(
    freq: u64,
    cmd_rx: Receiver<SdrCommand>,
    msg_tx: Sender<SdrEvent>,
    vox_active: Arc<AtomicBool>,
    audio_tx: Sender<AudioBlock>,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Open HackRF
    let hackrf = HackRf::open()?;

    // Configure initial RX settings
    let sample_rate_rx = 2_400_000;
    hackrf.set_sample_rate(sample_rate_rx)?;
    hackrf.set_baseband_filter_bandwidth(1_750_000)?;
    hackrf.set_freq(freq)?;
    hackrf.set_amp_enable(RX_AMP_ENABLED)?;
    hackrf.set_lna_gain(RX_LNA_GAIN_DB)?;
    hackrf.set_rxvga_gain(RX_VGA_GAIN_DB)?;

    // Create channel for processor
    let (to_processor, from_callback) = crossbeam_channel::bounded::<Vec<Complex<i8>>>(100);
    let (to_callback, from_processor) = crossbeam_channel::bounded::<Vec<Complex<i8>>>(100);

    let buffer_size = 131072;
    for _ in 0..100 {
        if to_callback.send(Vec::with_capacity(buffer_size)).is_err() {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "failed to seed HackRF RX recycle pool",
            )));
        }
    }

    let rx_ctx = TransceiverRxContext {
        to_processor,
        recycle_rx: from_processor,
        to_recycle_pool: to_callback.clone(),
        overrun_count: Arc::new(AtomicUsize::new(0)),
    };

    // Let's spawn the processor thread
    let msg_tx_clone = msg_tx.clone();
    let vox_active_clone = vox_active.clone();
    let audio_tx_clone = audio_tx.clone();
    std::thread::spawn(move || {
        let mut iq_accumulator = Complex::new(0.0f32, 0.0f32);
        let mut iq_count = 0;
        let mut prev_iq_48 = Complex::new(0.0f32, 0.0f32);
        let mut dc_prev_x = 0.0f32;
        let mut dc_prev_y = 0.0f32;
        let dc_alpha = 0.99f32;
        let mut deemp_prev_y = 0.0f32;
        let deemp_alpha = 0.757f32;
        let mut squelch = FskToneSquelch::new(
            FSK_SQUELCH_MIN_RMS,
            FSK_SQUELCH_OPEN_SCORE,
            FSK_SQUELCH_CLOSE_SCORE,
            FSK_SQUELCH_ATTACK_MS,
            FSK_SQUELCH_RELEASE_MS,
            modem::SAMPLE_RATE,
        );

        let mut pre_roll = Vec::new();
        let mut active_buffer = Vec::new();
        let pre_roll_limit = (200 * 48000) / 1000;

        let vox_block_size = 1024;
        let mut vox_block = Vec::with_capacity(vox_block_size);

        let mut base_image_buffers: std::collections::HashMap<u32, Vec<Option<Vec<u8>>>> =
            std::collections::HashMap::new();
        let mut arq_trackers: std::collections::HashMap<
            u32,
            (std::time::Instant, usize, usize, bool),
        > = std::collections::HashMap::new();

        while let Ok(buf) = from_callback.recv() {
            for current_iq in buf.iter() {
                let current_iq =
                    Complex::new(current_iq.re as f32 / 128.0, current_iq.im as f32 / 128.0);

                iq_accumulator += current_iq;
                iq_count += 1;
                if iq_count == 50 {
                    let iq_48 = iq_accumulator / 50.0;
                    iq_accumulator = Complex::new(0.0, 0.0);
                    iq_count = 0;

                    let conj_prev = Complex::new(prev_iq_48.re, -prev_iq_48.im);
                    let delta = iq_48 * conj_prev;
                    let fm_val_48 = delta.im.atan2(delta.re);
                    prev_iq_48 = iq_48;

                    let mut audio_sample = fm_val_48 * 0.30;

                    let cur_x = audio_sample;
                    let cur_y = dc_alpha * (dc_prev_y + cur_x - dc_prev_x);
                    dc_prev_x = cur_x;
                    dc_prev_y = cur_y;
                    audio_sample = cur_y;

                    let cur_deemp_y =
                        (1.0f32 - deemp_alpha) * audio_sample + deemp_alpha * deemp_prev_y;
                    deemp_prev_y = cur_deemp_y;
                    audio_sample = cur_deemp_y * 1.5;

                    pre_roll.push(audio_sample);
                    if pre_roll.len() > pre_roll_limit {
                        pre_roll.remove(0);
                    }

                    vox_block.push(audio_sample);
                    if vox_block.len() == vox_block_size {
                        let _ = audio_tx_clone.send(AudioBlock {
                            samples: vox_block.clone(),
                            direction: AudioDirection::Rx,
                        });
                        let prev_state = squelch.state;
                        let (_metrics, squelch_active) = squelch.process_block(&vox_block);
                        vox_active_clone.store(squelch_active, Ordering::Relaxed);

                        match (prev_state, squelch.state) {
                            (VoxState::Idle, VoxState::Active) => {
                                active_buffer.clear();
                                active_buffer.extend_from_slice(&pre_roll);
                                active_buffer.extend_from_slice(&vox_block);
                            }
                            (VoxState::Active, VoxState::Active) => {
                                active_buffer.extend_from_slice(&vox_block);
                            }
                            (VoxState::Active, VoxState::Idle) => {
                                let demod = Demodulator::new();
                                let fec_reports =
                                    demod.demodulate_multi_with_diagnostics(&active_buffer, true);
                                let fec_success =
                                    fec_reports.iter().any(|report| report.frame.is_some());
                                let reports = if fec_success {
                                    fec_reports
                                } else {
                                    let plain_reports = demod
                                        .demodulate_multi_with_diagnostics(&active_buffer, false);
                                    if plain_reports.iter().any(|report| report.frame.is_some())
                                        || fec_reports.is_empty()
                                    {
                                        plain_reports
                                    } else {
                                        fec_reports
                                    }
                                };

                                let decoded_frames = reports
                                    .iter()
                                    .filter(|report| report.frame.is_some())
                                    .count();
                                let failed_frames = if decoded_frames == 0 {
                                    1
                                } else {
                                    reports
                                        .iter()
                                        .filter(|report| report.frame.is_none())
                                        .count()
                                };
                                let (rssi_percent, rssi_dbm) = estimate_rssi(&active_buffer);
                                let telemetry = LinkTelemetry {
                                    snr_db: reports
                                        .iter()
                                        .filter_map(|report| report.snr_db)
                                        .max_by(|a, b| a.total_cmp(b)),
                                    rssi_dbm,
                                    rssi_percent,
                                    fec_corrections: reports
                                        .iter()
                                        .map(|report| report.fec_corrections)
                                        .sum(),
                                    crc_pass: if reports.is_empty() {
                                        None
                                    } else {
                                        Some(reports.iter().all(|report| report.crc_pass))
                                    },
                                    decoded_frames,
                                    failed_frames,
                                    sync_score: reports
                                        .iter()
                                        .map(|report| report.sync_score)
                                        .max_by(|a, b| a.total_cmp(b)),
                                    last_error: reports
                                        .iter()
                                        .rev()
                                        .find_map(|report| report.error.map(str::to_string)),
                                };
                                let _ = msg_tx_clone.send(SdrEvent::Telemetry(telemetry));

                                for frame in reports.into_iter().filter_map(|report| report.frame) {
                                    if frame.msg_type == modem::frame::MsgType::ImageChunk {
                                        match parse_image_chunk_payload(&frame.payload) {
                                            Ok(chunk) => {
                                                let image_id = chunk.image_id;
                                                let chunk_idx = chunk.chunk_idx as usize;
                                                let total_chunks = chunk.total_chunks as usize;

                                                let replace_existing =
                                                    base_image_buffers.get(&image_id).is_some_and(
                                                        |entry| entry.len() != total_chunks,
                                                    );
                                                if replace_existing {
                                                    let _ = msg_tx_clone.send(SdrEvent::Notice(format!(
                                                        "IMAGE_ERROR:Image ID {} changed size; resetting buffer",
                                                        image_id
                                                    )));
                                                    base_image_buffers.remove(&image_id);
                                                    arq_trackers.remove(&image_id);
                                                }

                                                let entry = base_image_buffers
                                                    .entry(image_id)
                                                    .or_insert_with(|| vec![None; total_chunks]);

                                                if chunk_idx < entry.len()
                                                    && entry[chunk_idx].is_none()
                                                {
                                                    entry[chunk_idx] = Some(chunk.data);
                                                }

                                                let count =
                                                    entry.iter().filter(|c| c.is_some()).count();
                                                let percent = if total_chunks == 0 {
                                                    0
                                                } else {
                                                    (count * 100) / total_chunks
                                                };
                                                let _ =
                                                    msg_tx_clone.send(SdrEvent::Notice(format!(
                                                        "IMAGE_PROGRESS:{} {}/{} ({}%)",
                                                        image_id, count, total_chunks, percent
                                                    )));

                                                // Each chunk takes ~3.6s; use 4.2s plus padding for ARQ timeout.
                                                let remaining_chunks =
                                                    total_chunks.saturating_sub(count);
                                                let eta_secs =
                                                    (remaining_chunks as f64 * 4.2) + 3.0;
                                                let eta_timeout = std::time::Instant::now()
                                                    + std::time::Duration::from_secs_f64(eta_secs);

                                                arq_trackers.insert(
                                                    image_id,
                                                    (eta_timeout, total_chunks, count, false),
                                                );

                                                if entry.iter().all(|c| c.is_some()) {
                                                    let mut full_binary = Vec::new();
                                                    for chunk_data in entry.iter().flatten() {
                                                        full_binary.extend_from_slice(chunk_data);
                                                    }
                                                    base_image_buffers.remove(&image_id);
                                                    arq_trackers.remove(&image_id);

                                                    let file_path = image_output_path(&full_binary);
                                                    match std::fs::write(file_path, &full_binary) {
                                                        Ok(_) => {
                                                            let _ = msg_tx_clone.send(
                                                                SdrEvent::Notice(format!(
                                                                    "IMAGE_COMPLETE:{}",
                                                                    file_path
                                                                )),
                                                            );
                                                        }
                                                        Err(e) => {
                                                            let _ = msg_tx_clone.send(SdrEvent::Notice(format!(
                                                                "IMAGE_ERROR:Failed to write image: {}",
                                                                e
                                                            )));
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                let _ =
                                                    msg_tx_clone.send(SdrEvent::Notice(format!(
                                                        "IMAGE_ERROR:Invalid image chunk: {}",
                                                        e
                                                    )));
                                            }
                                        }
                                    } else if let Ok(payload) = String::from_utf8(frame.payload) {
                                        let _ = msg_tx_clone.send(SdrEvent::Frame {
                                            msg_type: frame.msg_type,
                                            has_location: frame.has_location,
                                            payload,
                                        });
                                    }
                                }
                                active_buffer.clear();
                            }
                            (VoxState::Idle, VoxState::Idle) => {}
                        }
                        vox_block.clear();
                    }
                }
            }
            // Check for ARQ timeouts periodically (approx every 54ms)
            let now = std::time::Instant::now();
            for (&image_id, (eta_timeout, total_chunks, count, requested)) in
                arq_trackers.iter_mut()
            {
                if !*requested
                    && *count < *total_chunks
                    && now > *eta_timeout
                    && let Some(entry) = base_image_buffers.get(&image_id)
                {
                    let missing_indices: Vec<String> = entry
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.is_none())
                        .map(|(idx, _)| idx.to_string())
                        .collect();

                    if !missing_indices.is_empty() {
                        let indices_str = missing_indices.join(",");
                        let _ = msg_tx_clone.send(SdrEvent::Notice(format!(
                            "AUTOTX:REQ_CHUNKS {} {}",
                            image_id, indices_str
                        )));
                        *requested = true;
                    }
                }
            }

            let _ = to_callback.send(buf);
        }
    });

    // Start RX by default
    hackrf.start_rx(rx_callback_fn, rx_ctx.clone())?;

    // Main Control Loop
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            SdrCommand::Transmit(msg) => {
                transmit_frame(
                    &hackrf,
                    sample_rate_rx,
                    freq,
                    &rx_ctx,
                    msg_tx.clone(),
                    audio_tx.clone(),
                    MsgType::Response,
                    false,
                    msg,
                );
            }
            SdrCommand::TransmitFrame {
                msg_type,
                has_location,
                payload,
            } => {
                transmit_frame(
                    &hackrf,
                    sample_rate_rx,
                    freq,
                    &rx_ctx,
                    msg_tx.clone(),
                    audio_tx.clone(),
                    msg_type,
                    has_location,
                    payload,
                );
            }
        }
    }

    Ok(())
}

fn transmit_frame(
    hackrf: &HackRf,
    sample_rate_rx: u32,
    freq: u64,
    rx_ctx: &TransceiverRxContext,
    msg_tx: Sender<SdrEvent>,
    audio_tx: Sender<AudioBlock>,
    msg_type: MsgType,
    has_location: bool,
    msg: String,
) {
    // 1. Pause RX
    if let Err(e) = hackrf.stop_rx() {
        let _ = msg_tx.send(SdrEvent::Notice(format!(
            "System Error: Failed to stop RX before TX: {}",
            e
        )));
    }

    // 2. Prepare TX signal (AFSK modulation)
    if let Ok(frame) = Frame::new(1, msg_type, has_location, msg.into_bytes()) {
        let mut modulator = Modulator::new();
        let audio_samples = modulator.modulate(&frame, true, 500);

        let silence_len = 24000;
        let mut full_audio = Vec::with_capacity(audio_samples.len() + 2 * silence_len);
        full_audio.resize(silence_len, 0.0f32);
        full_audio.extend_from_slice(&audio_samples);
        full_audio.resize(full_audio.len() + silence_len, 0.0f32);

        let alpha = 0.98;
        let mut pre_emphasized = Vec::with_capacity(full_audio.len());
        let mut prev_x = 0.0f32;
        for &x in &full_audio {
            let y = x - alpha * prev_x;
            prev_x = x;
            pre_emphasized.push(y);
        }
        let max_peak = pre_emphasized
            .iter()
            .map(|x| x.abs())
            .fold(0.0f32, f32::max);
        if max_peak > 0.0 {
            for val in pre_emphasized.iter_mut() {
                *val /= max_peak;
            }
        }

        // FM Modulate to 2 MSPS IQ
        let sample_rate_rf = 2_000_000.0;
        let deviation_hz = 7000.0;
        let mut tx_samples =
            Vec::with_capacity(((pre_emphasized.len() as f64) * sample_rate_rf / 48000.0) as usize);

        let mut phase = 0.0f64;
        for n in 0.. {
            let t_audio = (n as f64) * 48000.0 / sample_rate_rf;
            let idx_audio = t_audio as usize;
            if idx_audio >= pre_emphasized.len() {
                break;
            }

            let frac = t_audio - idx_audio as f64;
            let s0 = pre_emphasized[idx_audio] as f64;
            let s1 = if idx_audio + 1 < pre_emphasized.len() {
                pre_emphasized[idx_audio + 1] as f64
            } else {
                s0
            };
            let interp_audio = s0 + frac * (s1 - s0);

            phase += 2.0 * std::f64::consts::PI * deviation_hz * interp_audio / sample_rate_rf;
            if phase >= 2.0 * std::f64::consts::PI {
                phase -= 2.0 * std::f64::consts::PI;
            } else if phase < 0.0 {
                phase += 2.0 * std::f64::consts::PI;
            }

            let re = (phase.cos() * 125.0).round() as i8;
            let im = (phase.sin() * 125.0).round() as i8;
            tx_samples.push(Complex::new(re, im));
        }

        let (done_sender, done_receiver) = crossbeam_channel::bounded::<()>(1);
        let duration_secs = tx_samples.len() as f64 / sample_rate_rf;

        let tx_ctx = TransceiverTxContext {
            samples: tx_samples,
            index: AtomicUsize::new(0),
            signaled: AtomicBool::new(false),
            done_sender,
        };

        // Switch settings for TX
        if let Err(e) = hackrf.set_sample_rate(sample_rate_rf as u32) {
            let _ = msg_tx.send(SdrEvent::Notice(format!(
                "System Error: Failed to set TX sample rate: {}",
                e
            )));
        }
        if let Err(e) = hackrf.set_txvga_gain(47) {
            let _ = msg_tx.send(SdrEvent::Notice(format!(
                "System Error: Failed to set TX VGA gain: {}",
                e
            )));
        }
        if let Err(e) = hackrf.set_amp_enable(true) {
            let _ = msg_tx.send(SdrEvent::Notice(format!(
                "System Error: Failed to enable TX amp: {}",
                e
            )));
        }

        // Start TX
        if let Ok(()) = hackrf.start_tx(tx_callback_fn, tx_ctx) {
            spawn_tx_audio_preview(audio_tx.clone(), pre_emphasized);
            let timeout_duration = Duration::from_secs_f64(duration_secs + 2.0);
            if let Err(e) = done_receiver.recv_timeout(timeout_duration) {
                let _ = msg_tx.send(SdrEvent::Notice(format!(
                    "System Error: TX completion timed out: {}",
                    e
                )));
            }
            if let Err(e) = hackrf.stop_tx() {
                let _ = msg_tx.send(SdrEvent::Notice(format!(
                    "System Error: Failed to stop TX: {}",
                    e
                )));
            }
        } else {
            let _ = msg_tx.send(SdrEvent::Notice(
                "System Error: Failed to start TX".to_string(),
            ));
        }
    }

    // 3. Resume RX
    if let Err(e) = hackrf.set_sample_rate(sample_rate_rx) {
        let _ = msg_tx.send(SdrEvent::Notice(format!(
            "System Error: Failed to restore RX sample rate: {}",
            e
        )));
    }
    if let Err(e) = hackrf.set_baseband_filter_bandwidth(1_750_000) {
        let _ = msg_tx.send(SdrEvent::Notice(format!(
            "System Error: Failed to restore RX filter: {}",
            e
        )));
    }
    if let Err(e) = hackrf.set_freq(freq) {
        let _ = msg_tx.send(SdrEvent::Notice(format!(
            "System Error: Failed to restore RX frequency: {}",
            e
        )));
    }
    if let Err(e) = hackrf.set_amp_enable(RX_AMP_ENABLED) {
        let _ = msg_tx.send(SdrEvent::Notice(format!(
            "System Error: Failed to restore RX amp: {}",
            e
        )));
    }
    if let Err(e) = hackrf.set_lna_gain(RX_LNA_GAIN_DB) {
        let _ = msg_tx.send(SdrEvent::Notice(format!(
            "System Error: Failed to restore RX LNA gain: {}",
            e
        )));
    }
    if let Err(e) = hackrf.set_rxvga_gain(RX_VGA_GAIN_DB) {
        let _ = msg_tx.send(SdrEvent::Notice(format!(
            "System Error: Failed to restore RX VGA gain: {}",
            e
        )));
    }

    if let Err(e) = hackrf.start_rx(rx_callback_fn, rx_ctx.clone()) {
        let _ = msg_tx.send(SdrEvent::Notice(format!(
            "System Error: Failed to resume RX: {}",
            e
        )));
    }
}
