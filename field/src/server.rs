use crate::audio;
use crate::state::{AppState, FieldLocation, VoxConfig};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::Html,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Deserialize)]
pub struct UpdateConfigPayload {
    pub device: Option<String>,
    pub vox_config: Option<VoxConfig>,
}

#[derive(Deserialize)]
pub struct TransmitPayload {
    pub message: String,
    pub use_fec: Option<bool>,
    pub tx_location: Option<bool>,
}

#[derive(Deserialize)]
pub struct TransmitImagePayload {
    pub image: String, // Base64 JPEG data URL
    pub use_fec: Option<bool>,
}

#[derive(Deserialize)]
pub struct LocationPayload {
    pub location: Option<FieldLocation>,
}

#[derive(Deserialize)]
pub struct SosPayload {
    pub message: Option<String>,
    pub use_fec: Option<bool>,
}

#[derive(Serialize)]
pub struct GenericResponse {
    pub status: String,
    pub message: Option<String>,
}

async fn get_status(State(state): State<AppState>) -> Json<crate::state::StatusResponse> {
    Json(state.get_status().await)
}

async fn update_config(
    State(state): State<AppState>,
    Json(payload): Json<UpdateConfigPayload>,
) -> (StatusCode, Json<GenericResponse>) {
    if let Some(dev) = payload.device {
        println!("Web Config: Updated ALSA device target to: {}", dev);
        state.update_device(dev).await;
    }
    if let Some(vox) = payload.vox_config {
        println!("Web Config: Updated Software VOX to {:?}", vox);
        state.update_vox(vox).await;
    }
    (
        StatusCode::OK,
        Json(GenericResponse {
            status: "success".to_string(),
            message: Some("Configuration updated successfully".to_string()),
        }),
    )
}

async fn transmit(
    State(state): State<AppState>,
    Json(payload): Json<TransmitPayload>,
) -> (StatusCode, Json<GenericResponse>) {
    let use_fec = payload.use_fec.unwrap_or(true);
    let tx_location = payload.tx_location.unwrap_or(false);
    let state_clone = state.clone();

    let mut message = payload.message.trim().to_string();
    let mut has_location = false;
    if tx_location && let Some(location) = state.get_location().await {
        let location_payload =
            modem::location::format_location_message(location.to_modem_location(), &message);
        message = format!("VK2EMM/P: {}", location_payload);
        has_location = true;
    } else if !message.is_empty() && !message.to_uppercase().starts_with("VK2EMM") {
        message = format!("VK2EMM/P: {}", message);
    }

    // Spawn transmission in an asynchronous task to keep API response instantaneous
    tokio::spawn(async move {
        if let Err(e) = audio::transmit_message(state_clone, message, use_fec, has_location).await {
            eprintln!("TX Error via Web API: {}", e);
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(GenericResponse {
            status: "accepted".to_string(),
            message: Some("Transmission scheduled".to_string()),
        }),
    )
}

async fn update_location(
    State(state): State<AppState>,
    Json(payload): Json<LocationPayload>,
) -> (StatusCode, Json<GenericResponse>) {
    match state.update_location(payload.location).await {
        Ok(()) => (
            StatusCode::OK,
            Json(GenericResponse {
                status: "success".to_string(),
                message: Some("Location updated".to_string()),
            }),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(GenericResponse {
                status: "error".to_string(),
                message: Some(e),
            }),
        ),
    }
}

async fn start_sos(
    State(state): State<AppState>,
    Json(payload): Json<SosPayload>,
) -> (StatusCode, Json<GenericResponse>) {
    let id = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        & 0xFFFF_FFFF)
        .to_string();
    let location = state.get_location().await;
    let message = payload
        .message
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    let use_fec = payload.use_fec.unwrap_or(true);

    state
        .start_sos(id.clone(), message.clone(), location.clone())
        .await;
    audio::spawn_sos_loop(state, id.clone(), message, location, use_fec);

    (
        StatusCode::ACCEPTED,
        Json(GenericResponse {
            status: "accepted".to_string(),
            message: Some(format!("SOS {} active", id)),
        }),
    )
}

async fn cancel_sos(State(state): State<AppState>) -> (StatusCode, Json<GenericResponse>) {
    let message = state
        .cancel_sos()
        .await
        .map(|id| format!("SOS {} cancelled locally", id))
        .unwrap_or_else(|| "No active SOS".to_string());

    (
        StatusCode::OK,
        Json(GenericResponse {
            status: "success".to_string(),
            message: Some(message),
        }),
    )
}

async fn transmit_image(
    State(state): State<AppState>,
    Json(payload): Json<TransmitImagePayload>,
) -> (StatusCode, Json<GenericResponse>) {
    match crate::state::base64_decode(&payload.image) {
        Ok(data) if data.is_empty() => {
            return (
                StatusCode::BAD_REQUEST,
                Json(GenericResponse {
                    status: "error".to_string(),
                    message: Some("Cannot transmit empty image".to_string()),
                }),
            );
        }
        Ok(data) if data.len() > modem::MAX_IMAGE_BYTES => {
            return (
                StatusCode::BAD_REQUEST,
                Json(GenericResponse {
                    status: "error".to_string(),
                    message: Some(format!(
                        "Image is too large: {} bytes exceeds {} bytes",
                        data.len(),
                        modem::MAX_IMAGE_BYTES
                    )),
                }),
            );
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(GenericResponse {
                    status: "error".to_string(),
                    message: Some(format!("Base64 decode failed: {}", e)),
                }),
            );
        }
    }

    let use_fec = payload.use_fec.unwrap_or(true);
    let state_clone = state.clone();

    // Spawn transmission in an asynchronous task to keep API response instantaneous
    tokio::spawn(async move {
        if let Err(e) = audio::transmit_image(state_clone, payload.image, use_fec).await {
            eprintln!("TX Image Error via Web API: {}", e);
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(GenericResponse {
            status: "accepted".to_string(),
            message: Some("Image transmission scheduled".to_string()),
        }),
    )
}

async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = state.subscribe();
    let stream = BroadcastStream::new(rx).map(|msg| match msg {
        Ok(text) => Ok(Event::default().data(text)),
        Err(_) => Ok(Event::default().data("system_overrun")),
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn hello_world() -> Html<&'static str> {
    Html(include_str!("chat.html"))
}

pub fn make_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(hello_world))
        .route("/events", get(sse_handler))
        .route("/status", get(get_status))
        .route("/config", post(update_config))
        .route("/location", post(update_location))
        .route("/transmit", post(transmit))
        .route("/transmit_image", post(transmit_image))
        .route("/sos/start", post(start_sos))
        .route("/sos/cancel", post(cancel_sos))
        .with_state(state)
}
