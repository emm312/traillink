use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

pub type SharedWebState = Arc<RwLock<BaseSnapshot>>;

#[derive(Clone)]
struct WebAppState {
    snapshot: SharedWebState,
    commands: Sender<WebCommand>,
}

#[derive(Debug, Clone)]
pub enum WebCommand {
    Transmit(String),
    AckSos,
    ClearSos,
    ClearLocation,
}

#[derive(Debug, Clone, Serialize)]
pub struct BaseSnapshot {
    pub updated_at_ms: u128,
    pub frequency_mhz: f64,
    pub mode: String,
    pub radio_state: String,
    pub audio_level: f64,
    pub image_status: Option<String>,
    pub claude_pending: usize,
    pub messages: Vec<WebMessage>,
    pub current_fft: Vec<f64>,
    pub waterfall_history: Vec<Vec<f64>>,
    pub base_location: Option<LocationSnapshot>,
    pub field_location: Option<LocationSnapshot>,
    pub active_sos: Option<SosSnapshot>,
    pub link: LinkSnapshot,
}

impl BaseSnapshot {
    pub fn empty() -> Self {
        Self {
            updated_at_ms: now_ms(),
            frequency_mhz: 0.0,
            mode: "Manual".to_string(),
            radio_state: "IDLE".to_string(),
            audio_level: 0.0,
            image_status: None,
            claude_pending: 0,
            messages: Vec::new(),
            current_fft: Vec::new(),
            waterfall_history: Vec::new(),
            base_location: None,
            field_location: None,
            active_sos: None,
            link: LinkSnapshot::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WebMessage {
    pub text: String,
    pub kind: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct LocationSnapshot {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: Option<f64>,
}

impl From<modem::location::Location> for LocationSnapshot {
    fn from(location: modem::location::Location) -> Self {
        Self {
            lat: location.lat,
            lon: location.lon,
            accuracy_m: location.accuracy_m,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SosSnapshot {
    pub id: String,
    pub call: Option<String>,
    pub message: Option<String>,
    pub location: Option<LocationSnapshot>,
    pub acknowledged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkSnapshot {
    pub snr_db: Option<f32>,
    pub rssi_dbm: Option<f32>,
    pub rssi_percent: f32,
    pub fec_corrections: usize,
    pub crc_pass: Option<bool>,
    pub sync_score: Option<f32>,
    pub packet_loss_percent: Option<f64>,
    pub round_trip_secs: Option<f64>,
    pub last_error: Option<String>,
    pub snr_history: Vec<f32>,
    pub rssi_history: Vec<f32>,
    pub burst_success: Vec<bool>,
}

impl Default for LinkSnapshot {
    fn default() -> Self {
        Self {
            snr_db: None,
            rssi_dbm: None,
            rssi_percent: 0.0,
            fec_corrections: 0,
            crc_pass: None,
            sync_score: None,
            packet_loss_percent: None,
            round_trip_secs: None,
            last_error: None,
            snr_history: Vec::new(),
            rssi_history: Vec::new(),
            burst_success: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct TransmitPayload {
    message: String,
}

#[derive(Serialize)]
struct GenericResponse {
    status: String,
    message: Option<String>,
}

pub fn new_shared_state() -> SharedWebState {
    Arc::new(RwLock::new(BaseSnapshot::empty()))
}

pub fn start_server(
    snapshot: SharedWebState,
    commands: Sender<WebCommand>,
) -> Result<SocketAddr, String> {
    let host = std::env::var("BASE_WEB_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("BASE_WEB_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8080);
    let addr_str = if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    };
    let addr: SocketAddr = addr_str
        .parse()
        .map_err(|_| format!("invalid BASE_WEB_HOST/BASE_WEB_PORT: {addr_str}"))?;
    let listener = std::net::TcpListener::bind(addr)
        .map_err(|error| format!("failed to bind base web dashboard at http://{addr}: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("failed to configure base web dashboard listener: {error}"))?;
    let bound_addr = listener
        .local_addr()
        .map_err(|error| format!("failed to read base web dashboard address: {error}"))?;

    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("Base web server failed to start Tokio runtime: {error}");
                return;
            }
        };

        runtime.block_on(async move {
            let app = make_router(snapshot, commands);
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!("Base web server failed to adopt listener: {error}");
                    return;
                }
            };
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("Base web server crashed: {error}");
            }
        });
    });

    Ok(bound_addr)
}

fn make_router(snapshot: SharedWebState, commands: Sender<WebCommand>) -> Router {
    let state = WebAppState { snapshot, commands };
    Router::new()
        .route("/", get(index))
        .route("/status", get(status))
        .route("/transmit", post(transmit))
        .route("/sos/ack", post(ack_sos))
        .route("/sos/clear", post(clear_sos))
        .route("/location/clear", post(clear_location))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("web.html"))
}

async fn status(State(state): State<WebAppState>) -> Json<BaseSnapshot> {
    let snapshot = state
        .snapshot
        .read()
        .map(|snapshot| snapshot.clone())
        .unwrap_or_else(|_| BaseSnapshot::empty());
    Json(snapshot)
}

async fn transmit(
    State(state): State<WebAppState>,
    Json(payload): Json<TransmitPayload>,
) -> (StatusCode, Json<GenericResponse>) {
    let message = payload.message.trim().to_string();
    if message.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(GenericResponse {
                status: "error".to_string(),
                message: Some("message cannot be empty".to_string()),
            }),
        );
    }

    send_command(&state, WebCommand::Transmit(message), "transmission queued")
}

async fn ack_sos(State(state): State<WebAppState>) -> (StatusCode, Json<GenericResponse>) {
    send_command(&state, WebCommand::AckSos, "SOS acknowledgement queued")
}

async fn clear_sos(State(state): State<WebAppState>) -> (StatusCode, Json<GenericResponse>) {
    send_command(&state, WebCommand::ClearSos, "SOS banner cleared")
}

async fn clear_location(State(state): State<WebAppState>) -> (StatusCode, Json<GenericResponse>) {
    send_command(&state, WebCommand::ClearLocation, "field location cleared")
}

fn send_command(
    state: &WebAppState,
    command: WebCommand,
    success_message: &str,
) -> (StatusCode, Json<GenericResponse>) {
    match state.commands.send(command) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(GenericResponse {
                status: "accepted".to_string(),
                message: Some(success_message.to_string()),
            }),
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(GenericResponse {
                status: "error".to_string(),
                message: Some("base station command loop is unavailable".to_string()),
            }),
        ),
    }
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
