use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const IMAGE_BUFFER_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoxConfig {
    pub threshold: f32,
    pub attack_ms: u64,
    pub release_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldLocation {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: Option<f64>,
}

impl FieldLocation {
    pub fn to_modem_location(&self) -> modem::location::Location {
        modem::location::Location {
            lat: self.lat,
            lon: self.lon,
            accuracy_m: self.accuracy_m,
        }
    }

    pub fn is_valid(&self) -> bool {
        (-90.0..=90.0).contains(&self.lat) && (-180.0..=180.0).contains(&self.lon)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SosStatus {
    pub active: bool,
    pub id: Option<String>,
    pub acknowledged: bool,
    pub message: Option<String>,
    pub location: Option<FieldLocation>,
}

#[derive(Debug, Clone)]
pub struct ActiveSos {
    pub id: String,
    pub message: Option<String>,
    pub location: Option<FieldLocation>,
    pub acknowledged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub device: String,
    pub vox_config: VoxConfig,
    pub current_rms: f32,
    pub vox_active: bool,
    pub is_transmitting: bool,
    pub rx_messages: Vec<String>,
    pub image_status: Option<String>,
    pub location: Option<FieldLocation>,
    pub sos: SosStatus,
}

#[derive(Debug, Clone)]
pub struct ImageBuffer {
    pub total_chunks: u16,
    pub chunks: Vec<Option<Vec<u8>>>,
    pub created_at: Instant,
}

pub struct InnerState {
    pub device: String,
    pub vox_config: VoxConfig,
    pub current_rms: f32,
    pub vox_active: bool,
    pub is_transmitting: bool,
    pub rx_messages: Vec<String>,
    pub image_buffers: HashMap<u32, ImageBuffer>,
    pub last_sent_image: Option<(u32, Vec<u8>)>,
    pub image_status: Option<String>,
    pub location: Option<FieldLocation>,
    pub active_sos: Option<ActiveSos>,
}

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<RwLock<InnerState>>,
    pub tx: tokio::sync::broadcast::Sender<String>,
}

pub fn base64_encode(data: &[u8]) -> String {
    const CHARSET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i] as usize;
        let b1 = if i + 1 < data.len() {
            data[i + 1] as usize
        } else {
            0
        };
        let b2 = if i + 2 < data.len() {
            data[i + 2] as usize
        } else {
            0
        };

        let val = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARSET[(val >> 18) & 0x3F] as char);
        result.push(CHARSET[(val >> 12) & 0x3F] as char);
        if i + 1 < data.len() {
            result.push(CHARSET[(val >> 6) & 0x3F] as char);
        } else {
            result.push('=');
        }
        if i + 2 < data.len() {
            result.push(CHARSET[val & 0x3F] as char);
        } else {
            result.push('=');
        }
        i += 3;
    }
    result
}

pub fn base64_decode(s: &str) -> Result<Vec<u8>, &'static str> {
    let s_clean = if let Some(idx) = s.find(',') {
        &s[idx + 1..]
    } else {
        s
    };

    const CHARSET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [0u8; 256];
    for (i, &b) in CHARSET.iter().enumerate() {
        lookup[b as usize] = i as u8;
    }

    let mut bytes = Vec::new();
    let mut buffer = 0u32;
    let mut bits_collected = 0;

    for &b in s_clean.as_bytes() {
        if b == b'=' {
            break;
        }
        if b.is_ascii_whitespace() {
            continue;
        }
        let val = lookup[b as usize];
        if val == 0 && b != b'A' {
            return Err("Invalid base64 character");
        }
        buffer = (buffer << 6) | (val as u32);
        bits_collected += 6;
        if bits_collected >= 8 {
            bits_collected -= 8;
            bytes.push(((buffer >> bits_collected) & 0xFF) as u8);
        }
    }
    Ok(bytes)
}

impl AppState {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(100);
        Self {
            inner: Arc::new(RwLock::new(InnerState {
                device: "plughw:CARD=Elite,DEV=0".to_string(),
                vox_config: VoxConfig {
                    threshold: 0.01,  // Default threshold, to be calibrated by user
                    attack_ms: 20,    // 20ms quick attack
                    release_ms: 1000, // 1 second release to prevent flutter
                },
                current_rms: 0.0,
                vox_active: false,
                is_transmitting: false,
                rx_messages: Vec::new(),
                image_buffers: HashMap::new(),
                last_sent_image: None,
                image_status: None,
                location: None,
                active_sos: None,
            })),
            tx,
        }
    }

    pub async fn get_status(&self) -> StatusResponse {
        let state = self.inner.read().await;
        StatusResponse {
            device: state.device.clone(),
            vox_config: state.vox_config.clone(),
            current_rms: state.current_rms,
            vox_active: state.vox_active,
            is_transmitting: state.is_transmitting,
            rx_messages: state.rx_messages.clone(),
            image_status: state.image_status.clone(),
            location: state.location.clone(),
            sos: state
                .active_sos
                .as_ref()
                .map(|sos| SosStatus {
                    active: true,
                    id: Some(sos.id.clone()),
                    acknowledged: sos.acknowledged,
                    message: sos.message.clone(),
                    location: sos.location.clone(),
                })
                .unwrap_or(SosStatus {
                    active: false,
                    id: None,
                    acknowledged: false,
                    message: None,
                    location: None,
                }),
        }
    }

    pub async fn update_vox(&self, config: VoxConfig) {
        let mut state = self.inner.write().await;
        state.vox_config = config;
    }

    pub async fn update_device(&self, device: String) {
        let mut state = self.inner.write().await;
        state.device = device;
    }

    pub async fn set_rms(&self, rms: f32) {
        let mut state = self.inner.write().await;
        state.current_rms = rms;
    }

    pub async fn set_vox_active(&self, active: bool) {
        let mut state = self.inner.write().await;
        state.vox_active = active;
    }

    pub async fn set_transmitting(&self, tx: bool) {
        let mut state = self.inner.write().await;
        state.is_transmitting = tx;
    }

    pub async fn add_message(&self, msg: String) {
        let mut state = self.inner.write().await;
        state.rx_messages.push(msg.clone());
        if state.rx_messages.len() > 100 {
            state.rx_messages.remove(0);
        }
        let _ = self.tx.send(msg);
    }

    pub async fn update_location(&self, location: Option<FieldLocation>) -> Result<(), String> {
        if let Some(location) = &location
            && !location.is_valid()
        {
            return Err("Location coordinates are out of range".to_string());
        }
        let mut state = self.inner.write().await;
        state.location = location;
        Ok(())
    }

    pub async fn get_location(&self) -> Option<FieldLocation> {
        let state = self.inner.read().await;
        state.location.clone()
    }

    pub async fn start_sos(
        &self,
        id: String,
        message: Option<String>,
        location: Option<FieldLocation>,
    ) {
        let mut state = self.inner.write().await;
        state.active_sos = Some(ActiveSos {
            id: id.clone(),
            message: message.filter(|value| !value.trim().is_empty()),
            location,
            acknowledged: false,
        });
        let _ = self.tx.send(format!("SOS_ACTIVE:{}", id));
    }

    pub async fn cancel_sos(&self) -> Option<String> {
        let mut state = self.inner.write().await;
        let id = state.active_sos.take().map(|sos| sos.id);
        if let Some(id) = &id {
            let _ = self.tx.send(format!("SOS_CANCELLED:{}", id));
        }
        id
    }

    pub async fn acknowledge_sos(&self, id: &str) -> bool {
        let mut state = self.inner.write().await;
        let matches = state
            .active_sos
            .as_ref()
            .is_some_and(|sos| sos.id == id.trim());
        if matches {
            if let Some(sos) = &mut state.active_sos {
                sos.acknowledged = true;
            }
            state.active_sos = None;
            let _ = self.tx.send(format!("SOS_ACK:{}", id.trim()));
            true
        } else {
            false
        }
    }

    pub async fn active_sos_id(&self) -> Option<String> {
        let state = self.inner.read().await;
        state.active_sos.as_ref().map(|sos| sos.id.clone())
    }

    pub async fn handle_image_chunk(&self, payload: &[u8]) {
        let chunk = match modem::image::parse_image_chunk_payload(payload) {
            Ok(chunk) => chunk,
            Err(e) => {
                eprintln!("Invalid ImageChunk payload: {}", e);
                return;
            }
        };

        if chunk.total_chunks == 0 {
            eprintln!("Invalid ImageChunk payload: zero total chunks");
            return;
        }

        let mut state = self.inner.write().await;

        let now = Instant::now();
        state
            .image_buffers
            .retain(|_, buffer| now.duration_since(buffer.created_at) <= IMAGE_BUFFER_TTL);

        let replace_existing = state
            .image_buffers
            .get(&chunk.image_id)
            .is_some_and(|buffer| buffer.total_chunks != chunk.total_chunks);

        if replace_existing {
            eprintln!(
                "ImageChunk ID {} total changed; discarding stale buffer",
                chunk.image_id
            );
            state.image_buffers.remove(&chunk.image_id);
        }

        let mut completed_binary = None;
        let progress_status = {
            let entry = state
                .image_buffers
                .entry(chunk.image_id)
                .or_insert_with(|| {
                    println!(
                        "Initializing ImageBuffer for ID {} with {} chunks",
                        chunk.image_id, chunk.total_chunks
                    );
                    ImageBuffer {
                        total_chunks: chunk.total_chunks,
                        chunks: vec![None; chunk.total_chunks as usize],
                        created_at: now,
                    }
                });

            let chunk_idx = chunk.chunk_idx as usize;
            if chunk_idx >= entry.chunks.len() {
                eprintln!(
                    "ImageChunk index {} out of range for total_chunks {}",
                    chunk_idx,
                    entry.chunks.len()
                );
                return;
            }

            entry.chunks[chunk_idx] = Some(chunk.data);
            let received_chunks = entry.chunks.iter().filter(|c| c.is_some()).count();
            let total_chunks = usize::from(entry.total_chunks);
            let percent = if total_chunks == 0 {
                0
            } else {
                (received_chunks * 100) / total_chunks
            };

            println!(
                "Received ImageChunk ID {} [{} of {}]",
                chunk.image_id,
                chunk_idx + 1,
                entry.total_chunks
            );

            // Check if all chunks are present
            if entry.chunks.iter().all(|c| c.is_some()) {
                let mut full_binary = Vec::new();
                for chunk_data in entry.chunks.iter().flatten() {
                    full_binary.extend_from_slice(chunk_data);
                }
                completed_binary = Some(full_binary);
            }

            format!(
                "{} {}/{} ({}%)",
                chunk.image_id, received_chunks, total_chunks, percent
            )
        };
        state.image_status = Some(progress_status);

        if let Some(full_binary) = completed_binary {
            println!("Image ID {} fully reassembled!", chunk.image_id);

            // Remove buffer to prevent memory growth
            state.image_buffers.remove(&chunk.image_id);
            state.image_status = Some(format!("{} complete", chunk.image_id));

            // Encode binary bytes as base64
            let base64_data = base64_encode(&full_binary);
            let sse_message = format!("IMAGE:data:image/jpeg;base64,{}", base64_data);

            // Temporarily drop lock to call add_message (which obtains write lock)
            drop(state);
            self.add_message(sse_message).await;
        }
    }

    pub async fn set_last_sent_image(&self, image_id: u32, data: Vec<u8>) {
        let mut state = self.inner.write().await;
        state.last_sent_image = Some((image_id, data));
    }

    pub async fn get_last_sent_chunks(
        &self,
        image_id: u32,
        indices: Vec<u16>,
    ) -> Option<Vec<(u16, Vec<u8>)>> {
        let state = self.inner.read().await;
        if let Some((saved_id, ref data)) = state.last_sent_image
            && saved_id == image_id
        {
            let chunk_size = modem::IMAGE_CHUNK_DATA_BYTES;
            let mut chunks = Vec::new();
            for idx in indices {
                let start = idx as usize * chunk_size;
                if start < data.len() {
                    let end = std::cmp::min(start + chunk_size, data.len());
                    chunks.push((idx, data[start..end].to_vec()));
                }
            }
            return Some(chunks);
        }
        None
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.tx.subscribe()
    }
}
