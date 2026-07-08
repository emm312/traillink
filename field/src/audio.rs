use crate::state::AppState;
use modem::demodulator::Demodulator;
use modem::frame::{Frame, MsgType};
use modem::image::{ImageChunk, encode_image_chunk_payload, image_chunk_count};
use modem::vox::{FskToneSquelch, VoxState};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const SOS_REPEAT_SECS: u64 = 15;
const FSK_SQUELCH_OPEN_SCORE: f32 = 0.62;
const FSK_SQUELCH_CLOSE_SCORE: f32 = 0.54;

fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|chunk| {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            sample as f32 / i16::MAX as f32
        })
        .collect()
}

fn f32_to_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let val = (clamped * i16::MAX as f32) as i16;
        bytes.extend_from_slice(&val.to_le_bytes());
    }
    bytes
}

async fn play_raw_audio(device: &str, bytes: &[u8]) -> Result<(), String> {
    let mut child = Command::new("aplay")
        .args([
            "-D", device, "-f", "S16_LE", "-r", "48000", "-t", "raw", "-c", "1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn aplay: {}", e))?;

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to open aplay stdin".to_string())?;
        if let Err(e) = stdin.write_all(bytes).await {
            let _ = child.kill().await;
            return Err(format!("Failed writing to aplay: {}", e));
        }
        if let Err(e) = stdin.flush().await {
            let _ = child.kill().await;
            return Err(format!("Failed flushing aplay stdin: {}", e));
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed waiting for aplay: {}", e))?;
    if !status.success() {
        return Err(format!("aplay exited with status: {}", status));
    }

    Ok(())
}

async fn play_samples(state: &AppState, samples: &[f32], context: &str) -> Result<(), String> {
    let bytes = f32_to_bytes(samples);
    let device = {
        let s = state.inner.read().await;
        s.device.clone()
    };

    println!(
        "Spawning aplay for {} of {} bytes on device: {}",
        context,
        bytes.len(),
        device
    );

    state.set_transmitting(true).await;
    let result = play_raw_audio(&device, &bytes).await;
    state.set_transmitting(false).await;
    result
}

pub fn spawn_audio_loops(state: AppState) {
    tokio::spawn(async move {
        loop {
            let device = {
                let s = state.inner.read().await;
                s.device.clone()
            };

            println!("Spawning arecord capture on device: {}", device);

            let mut child = match Command::new("arecord")
                .args([
                    "-D", &device, "-f", "S16_LE", "-r", "48000", "-t", "raw", "-c", "1",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "Failed to spawn arecord: {}. Retrying in 5 seconds (maybe running on non-Linux?).",
                        e
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            let Some(mut stdout) = child.stdout.take() else {
                eprintln!("Failed to open arecord stdout. Restarting capture stream...");
                let _ = child.kill().await;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            };
            let mut buf = vec![0u8; 1024]; // 512 samples = 10.67ms at 48kHz

            let mut vox = {
                let s = state.inner.read().await;
                FskToneSquelch::new(
                    s.vox_config.threshold,
                    FSK_SQUELCH_OPEN_SCORE,
                    FSK_SQUELCH_CLOSE_SCORE,
                    s.vox_config.attack_ms,
                    s.vox_config.release_ms,
                    modem::SAMPLE_RATE,
                )
            };

            let mut sliding_buffer = Vec::new();
            let mut decoded_history: Vec<Vec<u8>> = Vec::new();
            let demod = Demodulator::new();

            let mut leftover_len = 0;

            while let Ok(n) = stdout.read(&mut buf[leftover_len..]).await {
                if n == 0 {
                    break; // EOF
                }

                let total_bytes = leftover_len + n;
                let valid_bytes = total_bytes - (total_bytes % 2);
                let samples = bytes_to_f32(&buf[..valid_bytes]);

                // Carry over the odd byte if present
                if total_bytes % 2 != 0 {
                    buf[0] = buf[total_bytes - 1];
                    leftover_len = 1;
                } else {
                    leftover_len = 0;
                }

                // Avoid processing sidetone / loopback while transmitting
                let is_tx = {
                    let s = state.inner.read().await;
                    s.is_transmitting
                };
                if is_tx {
                    vox.state = VoxState::Idle;
                    sliding_buffer.clear();
                    continue;
                }

                // Update VOX parameters dynamically in case changed by user
                {
                    let s = state.inner.read().await;
                    vox.update_config(
                        s.vox_config.threshold,
                        FSK_SQUELCH_OPEN_SCORE,
                        FSK_SQUELCH_CLOSE_SCORE,
                        s.vox_config.attack_ms,
                        s.vox_config.release_ms,
                        modem::SAMPLE_RATE,
                    );
                }

                let prev_active = {
                    let s = state.inner.read().await;
                    s.vox_active
                };
                let (metrics, vox_active) = vox.process_block(&samples);

                // Update shared metrics for UI/Web telemetry
                state.set_rms(metrics.rms).await;
                state.set_vox_active(vox_active).await;

                // Handle VOX state transitions (edges) and buffer management
                if vox_active != prev_active {
                    if vox_active {
                        println!(
                            "VOX Gate: OPENED (Signal detected). Starting continuous sliding scan."
                        );
                    } else {
                        // Falling edge: VOX Gate closed.
                        // Perform one final scan of the entire buffered transmission
                        println!("VOX Gate: CLOSED (Signal ended). Running final scan...");

                        // 1. Try decoding with FEC (standard)
                        let frames = demod.demodulate_multi(&sliding_buffer, true);
                        for frame in frames {
                            let raw_bytes = frame.to_bytes();
                            if !decoded_history.contains(&raw_bytes) {
                                decoded_history.push(raw_bytes);
                                println!("Decoded Final Frame (FEC): {:?}", frame.msg_type);
                                let state_async = state.clone();
                                tokio::spawn(async move {
                                    handle_decoded_frame(state_async, frame).await;
                                });
                            } else if frame.msg_type == MsgType::ImageChunk {
                                println!("Decoded ImageChunk Frame (FEC - Duplicate)");
                            } else if let Ok(payload) = String::from_utf8(frame.payload.clone()) {
                                println!("Decoded Final Frame (FEC - Duplicate): {}", payload);
                            }
                        }

                        // 2. Try decoding without FEC (fallback)
                        let frames_no_fec = demod.demodulate_multi(&sliding_buffer, false);
                        for frame in frames_no_fec {
                            let raw_bytes = frame.to_bytes();
                            if !decoded_history.contains(&raw_bytes) {
                                decoded_history.push(raw_bytes);
                                println!("Decoded Final Frame (No FEC): {:?}", frame.msg_type);
                                let state_async = state.clone();
                                tokio::spawn(async move {
                                    handle_decoded_frame(state_async, frame).await;
                                });
                            } else if frame.msg_type == MsgType::ImageChunk {
                                println!("Decoded ImageChunk Frame (No FEC - Duplicate)");
                            } else if let Ok(payload) = String::from_utf8(frame.payload.clone()) {
                                println!("Decoded Final Frame (No FEC - Duplicate): {}", payload);
                            }
                        }

                        sliding_buffer.clear();
                    }
                }

                if vox_active {
                    // While VOX is active, continuously append the incoming audio
                    sliding_buffer.extend_from_slice(&samples);

                    // Keep sliding buffer capped at 120 seconds (2 minutes) to handle large continuous streams (like images) without truncation
                    let max_samples = 120 * 48000;
                    if sliding_buffer.len() > max_samples {
                        let excess = sliding_buffer.len() - max_samples;
                        sliding_buffer.drain(..excess);
                    }
                } else {
                    // While VOX is inactive, keep sliding buffer empty and clean
                    sliding_buffer.clear();
                }
            }

            println!("arecord capture process ended. Restarting stream...");
            let _ = child.kill().await;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
}

async fn handle_decoded_frame(state: AppState, frame: Frame) {
    if frame.msg_type == MsgType::ImageChunk {
        println!("Decoded ImageChunk Frame");
        state.handle_image_chunk(&frame.payload).await;
        return;
    }

    let Ok(payload) = String::from_utf8(frame.payload) else {
        eprintln!("Decoded non-UTF8 frame payload for {:?}", frame.msg_type);
        return;
    };

    if frame.msg_type == MsgType::Ack
        && let Some(id) = payload.strip_prefix("SOS_ACK:")
    {
        if state.acknowledge_sos(id).await {
            state
                .add_message(format!("System: SOS acknowledged by base ({})", id.trim()))
                .await;
        }
        return;
    }

    state.add_message(payload.clone()).await;
    if payload.contains("REQ_CHUNKS") {
        handle_arq_request(state, payload).await;
    }
}

pub async fn transmit_message(
    state: AppState,
    message: String,
    use_fec: bool,
    has_location: bool,
) -> Result<(), String> {
    transmit_frame(
        state,
        MsgType::Broadcast,
        has_location,
        message,
        use_fec,
        750,
        "message transmission",
    )
    .await
}

pub async fn transmit_frame(
    state: AppState,
    msg_type: MsgType,
    has_location: bool,
    payload: String,
    use_fec: bool,
    preamble_ms: usize,
    context: &str,
) -> Result<(), String> {
    let frame = Frame::new(1, msg_type, has_location, payload.into_bytes())
        .map_err(|e| format!("Failed to create frame: {}", e))?;
    let mut modulator = modem::modulator::Modulator::new();
    let samples = modulator.modulate(&frame, use_fec, preamble_ms);

    play_samples(&state, &samples, context).await?;
    println!("Transmission playback finished.");
    Ok(())
}

pub fn build_sos_payload(
    id: &str,
    message: Option<&str>,
    location: Option<&crate::state::FieldLocation>,
) -> String {
    let mut payload = format!("SOS:{};CALL:VK2EMM/P", id);
    if let Some(location) = location {
        payload.push(';');
        payload.push_str(&modem::location::format_location(
            location.to_modem_location(),
        ));
    }
    if let Some(message) = message.map(str::trim).filter(|m| !m.is_empty()) {
        payload.push_str(";MSG:");
        payload.push_str(message);
    }
    payload
}

pub fn spawn_sos_loop(
    state: AppState,
    id: String,
    message: Option<String>,
    location: Option<crate::state::FieldLocation>,
    use_fec: bool,
) {
    tokio::spawn(async move {
        loop {
            let Some(active_id) = state.active_sos_id().await else {
                break;
            };
            if active_id != id {
                break;
            }

            let payload = build_sos_payload(&id, message.as_deref(), location.as_ref());
            if let Err(e) = transmit_frame(
                state.clone(),
                MsgType::SOS,
                location.is_some(),
                payload,
                use_fec,
                1200,
                "SOS transmission",
            )
            .await
            {
                eprintln!("SOS TX Error: {}", e);
                state
                    .add_message(format!("System Error: SOS transmission failed: {}", e))
                    .await;
            }

            tokio::time::sleep(std::time::Duration::from_secs(SOS_REPEAT_SECS)).await;
        }
    });
}

pub async fn transmit_image(
    state: AppState,
    image_data_url: String,
    use_fec: bool,
) -> Result<(), String> {
    // 1. Decode image base64
    let binary_data = crate::state::base64_decode(&image_data_url)
        .map_err(|e| format!("Base64 decode failed: {}", e))?;

    if binary_data.is_empty() {
        return Err("Cannot transmit empty image".to_string());
    }
    if binary_data.len() > modem::MAX_IMAGE_BYTES {
        return Err(format!(
            "Image is too large: {} bytes exceeds {} bytes",
            binary_data.len(),
            modem::MAX_IMAGE_BYTES
        ));
    }

    // 2. Split into robust 250-byte chunks to ensure reliable over-the-air decoding,
    // avoiding timing clock-drift and radio TOT limits on long continuous transmissions.
    let chunk_size = modem::IMAGE_CHUNK_DATA_BYTES;
    let total_chunks = image_chunk_count(binary_data.len())
        .map_err(|e| format!("Failed to chunk image: {}", e))?;

    let image_id = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        & 0xFFFF_FFFF) as u32;

    // Cache this image data in the state for selective ARQ retransmission
    state
        .set_last_sent_image(image_id, binary_data.clone())
        .await;

    println!(
        "Preparing image ID {} for transmission ({} bytes, {} chunks)",
        image_id,
        binary_data.len(),
        total_chunks
    );

    let mut all_samples = Vec::new();
    let mut modulator = modem::modulator::Modulator::new();

    for chunk_idx in 0..usize::from(total_chunks) {
        let start = chunk_idx * chunk_size;
        let end = std::cmp::min((chunk_idx + 1) * chunk_size, binary_data.len());
        let chunk_data = &binary_data[start..end];

        let payload = encode_image_chunk_payload(&ImageChunk {
            image_id,
            chunk_idx: chunk_idx as u16,
            total_chunks,
            data: chunk_data.to_vec(),
        })
        .map_err(|e| format!("Failed to encode image chunk: {}", e))?;

        let frame = modem::frame::Frame::new(1, modem::frame::MsgType::ImageChunk, false, payload)
            .map_err(|e| format!("Failed to create frame: {}", e))?;

        // Use a longer preamble (1200ms) for the first chunk to allow the radio squelch and AGC to open/settle,
        // and shorter preambles (400ms) for subsequent chunks to speed up transmission and reduce airtime
        let preamble_ms = if chunk_idx == 0 { 1200 } else { 400 };
        let samples = modulator.modulate(&frame, use_fec, preamble_ms);
        all_samples.extend_from_slice(&samples);

        // Add 300 ms of silence (zeros) between chunks to allow the receiver plenty of time to process, reset, and settle
        all_samples.extend(vec![0.0f32; 14400]);
    }

    play_samples(
        &state,
        &all_samples,
        &format!("image transmission ID {}", image_id),
    )
    .await?;
    println!("Image transmission playback finished.");
    Ok(())
}

pub async fn handle_arq_request(state: AppState, payload: String) {
    if let Some(idx) = payload.find("REQ_CHUNKS") {
        let req_part = &payload[idx + 10..]; // Skip "REQ_CHUNKS"
        let parts: Vec<&str> = req_part.split_whitespace().collect();
        if parts.len() == 2
            && let (Ok(image_id), Some(indices_str)) = (parts[0].parse::<u32>(), parts.get(1))
        {
            let mut indices = Vec::new();
            for idx_s in indices_str.split(',') {
                if let Ok(i) = idx_s.parse::<u16>() {
                    indices.push(i);
                }
            }

            println!(
                "ARQ Request received for image ID {} chunks {:?}",
                image_id, indices
            );
            if let Some(chunks_to_send) = state.get_last_sent_chunks(image_id, indices).await {
                let total_chunks_opt = {
                    let s = state.inner.read().await;
                    s.last_sent_image
                        .as_ref()
                        .map(|(_, d)| d.len().div_ceil(modem::IMAGE_CHUNK_DATA_BYTES))
                };
                if let Some(total_chunks) = total_chunks_opt {
                    let mut all_samples = Vec::new();
                    let mut modulator = modem::modulator::Modulator::new();
                    for (i, (chunk_idx, chunk_data)) in chunks_to_send.into_iter().enumerate() {
                        let total_chunks = match u16::try_from(total_chunks) {
                            Ok(total_chunks) => total_chunks,
                            Err(_) => {
                                eprintln!("ARQ total chunks exceeds u16 range");
                                return;
                            }
                        };

                        let payload = match encode_image_chunk_payload(&ImageChunk {
                            image_id,
                            chunk_idx,
                            total_chunks,
                            data: chunk_data,
                        }) {
                            Ok(payload) => payload,
                            Err(e) => {
                                eprintln!("Failed to encode ARQ image chunk: {}", e);
                                continue;
                            }
                        };

                        if let Ok(frame) = modem::frame::Frame::new(
                            1,
                            modem::frame::MsgType::ImageChunk,
                            false,
                            payload,
                        ) {
                            // Dynamic preamble: 1200ms for first chunk of the ARQ burst, 400ms for subsequent chunks
                            let preamble_ms = if i == 0 { 1200 } else { 400 };
                            let samples = modulator.modulate(&frame, true, preamble_ms); // Send with FEC
                            all_samples.extend_from_slice(&samples);
                            // Add 300 ms of silence (zeros) between chunks
                            all_samples.extend(vec![0.0f32; 14400]);
                        }
                    }

                    if !all_samples.is_empty() {
                        println!("ARQ: Transmitting requested chunks over the air...");
                        if let Err(e) =
                            play_samples(&state, &all_samples, "ARQ chunk retransmission").await
                        {
                            eprintln!("ARQ retransmission failed: {}", e);
                            state
                                .add_message(format!(
                                    "System Error: ARQ retransmission failed: {}",
                                    e
                                ))
                                .await;
                        }
                    }
                }
            }
        }
    }
}
