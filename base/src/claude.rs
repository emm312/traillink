use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

const DEFAULT_MODEL: &str = "claude-fable-5";
const DEFAULT_MAX_TOKENS: u32 = 180;
const DEFAULT_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyMode {
    Manual,
    Claude,
}

impl ReplyMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Manual => Self::Claude,
            Self::Claude => Self::Manual,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::Claude => "Claude",
        }
    }
}

impl std::str::FromStr for ReplyMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "manual" | "human" => Ok(Self::Manual),
            "claude" => Ok(Self::Claude),
            other => Err(format!("invalid reply mode: {}", other)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    pub messages_url: String,
}

impl ClaudeConfig {
    pub fn from_env() -> Result<Self, String> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| "ANTHROPIC_API_KEY is not set".to_string())?;
        let api_key = api_key.trim().to_string();
        if api_key.is_empty() {
            return Err("ANTHROPIC_API_KEY is empty".to_string());
        }

        let model = std::env::var("ANTHROPIC_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let max_tokens = std::env::var("ANTHROPIC_MAX_TOKENS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_TOKENS);
        let messages_url = std::env::var("ANTHROPIC_MESSAGES_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MESSAGES_URL.to_string());

        Ok(Self {
            api_key,
            model,
            max_tokens,
            messages_url,
        })
    }
}

#[derive(Debug, Serialize)]
struct ClaudeRequest {
    model: String,
    max_tokens: u32,
    system: String,
    messages: Vec<ClaudeMessage>,
    tools: Vec<WebSearchTool>,
}

#[derive(Debug, Serialize)]
struct ClaudeMessage {
    role: &'static str,
    content: ClaudeMessageContent,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ClaudeMessageContent {
    Text(String),
    Blocks(Vec<ClaudeInputBlock>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ClaudeInputBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ClaudeImageSource },
}

#[derive(Debug, Serialize)]
struct ClaudeImageSource {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: &'static str,
    data: String,
}

#[derive(Debug, Serialize)]
struct WebSearchTool {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'static str,
    max_uses: u8,
}

#[derive(Debug, Deserialize)]
struct ClaudeResponse {
    content: Vec<ClaudeContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ClaudeContentBlock {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

pub fn mode_from_env() -> Result<ReplyMode, String> {
    match std::env::var("BASE_REPLY_MODE") {
        Ok(value) => value.parse(),
        Err(_) => Ok(ReplyMode::Manual),
    }
}

pub fn ask_claude(config: &ClaudeConfig, field_message: &str) -> Result<String, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|e| format!("failed to build Claude HTTP client: {}", e))?;

    let request = build_request(config, field_message);
    let response = client
        .post(&config.messages_url)
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .json(&request)
        .send()
        .map_err(|e| format!("Claude API request failed: {}", e))?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .unwrap_or_else(|_| "<failed to read error body>".to_string());
        return Err(format!("Claude API returned {}: {}", status, body));
    }

    let body: ClaudeResponse = response
        .json()
        .map_err(|e| format!("failed to parse Claude API response: {}", e))?;

    extract_text(body).ok_or_else(|| "Claude API response did not include text".to_string())
}

pub fn ask_claude_about_image(
    config: &ClaudeConfig,
    image_path: &str,
    context: Option<&str>,
) -> Result<String, String> {
    let bytes =
        std::fs::read(image_path).map_err(|e| format!("failed to read image for Claude: {}", e))?;
    if bytes.is_empty() {
        return Err("received image is empty".to_string());
    }
    let media_type = image_media_type(image_path, &bytes)?;
    let prompt = match context {
        Some(context) if !context.trim().is_empty() => format!(
            "The field station sent this image over TrailLink. Briefly describe what matters for the field operator. Context: {}",
            context.trim()
        ),
        _ => "The field station sent this image over TrailLink. Briefly describe what matters for the field operator.".to_string(),
    };

    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("failed to build Claude HTTP client: {}", e))?;

    let request = build_image_request(config, prompt, media_type, BASE64_STANDARD.encode(bytes));
    let response = client
        .post(&config.messages_url)
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .json(&request)
        .send()
        .map_err(|e| format!("Claude API image request failed: {}", e))?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .unwrap_or_else(|_| "<failed to read error body>".to_string());
        return Err(format!("Claude API returned {}: {}", status, body));
    }

    let body: ClaudeResponse = response
        .json()
        .map_err(|e| format!("failed to parse Claude API image response: {}", e))?;

    extract_text(body).ok_or_else(|| "Claude API image response did not include text".to_string())
}

fn build_request(config: &ClaudeConfig, field_message: &str) -> ClaudeRequest {
    ClaudeRequest {
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        system: system_prompt(field_message),
        messages: vec![ClaudeMessage {
            role: "user",
            content: ClaudeMessageContent::Text(field_message.trim().to_string()),
        }],
        tools: vec![WebSearchTool {
            kind: "web_search_20250305",
            name: "web_search",
            max_uses: 3,
        }],
    }
}

fn build_image_request(
    config: &ClaudeConfig,
    prompt: String,
    media_type: &'static str,
    image_data: String,
) -> ClaudeRequest {
    ClaudeRequest {
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        system: system_prompt(&prompt),
        messages: vec![ClaudeMessage {
            role: "user",
            content: ClaudeMessageContent::Blocks(vec![
                ClaudeInputBlock::Text {
                    text: prompt.trim().to_string(),
                },
                ClaudeInputBlock::Image {
                    source: ClaudeImageSource {
                        kind: "base64",
                        media_type,
                        data: image_data,
                    },
                },
            ]),
        }],
        tools: vec![WebSearchTool {
            kind: "web_search_20250305",
            name: "web_search",
            max_uses: 3,
        }],
    }
}

fn extract_text(response: ClaudeResponse) -> Option<String> {
    let text = response
        .content
        .into_iter()
        .filter(|block| block.kind == "text")
        .filter_map(|block| block.text)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if text.is_empty() { None } else { Some(text) }
}

pub fn trim_for_radio(text: &str) -> String {
    let mut cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let max_len = modem::MAX_FRAME_PAYLOAD_BYTES.saturating_sub("VK2EMM: ".len());
    if cleaned.len() <= max_len {
        return cleaned;
    }

    while cleaned.len() > max_len {
        cleaned.pop();
    }
    cleaned.trim_end().to_string()
}

fn system_prompt(field_message: &str) -> String {
    let location_hint = extract_location_hint(field_message)
        .map(|loc| format!(" Field location hint: {}.", loc))
        .unwrap_or_default();

    format!(
        "You are TrailLink, an off-grid assistant replying over a very low bandwidth amateur radio link. Reply in terse plain text, preferably under 80 words. Use web search for current, local, weather, route, emergency-service, or time-sensitive questions. Avoid markdown tables, long lists, code fences, private/sensitive claims, and any obfuscation or encryption. Include only what is useful to the field operator.{}",
        location_hint
    )
}

fn extract_location_hint(text: &str) -> Option<String> {
    if let Some(parsed) = modem::location::parse_location_message(text) {
        return Some(match parsed.location.accuracy_m {
            Some(acc) => format!(
                "lat {:.6}, lon {:.6}, accuracy {:.0}m",
                parsed.location.lat, parsed.location.lon, acc
            ),
            None => format!(
                "lat {:.6}, lon {:.6}",
                parsed.location.lat, parsed.location.lon
            ),
        });
    }

    let lower = text.to_ascii_lowercase();
    let has_location_words =
        lower.contains("lat") || lower.contains("lon") || lower.contains("lng");
    if !has_location_words {
        return None;
    }

    Some(text.trim().chars().take(160).collect())
}

fn image_media_type(path: &str, bytes: &[u8]) -> Result<&'static str, String> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Ok("image/jpeg");
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Ok("image/png");
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Ok("image/webp");
    }

    match Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        Some("png") => Ok("image/png"),
        Some("webp") => Ok("image/webp"),
        _ => Err("unsupported image type for Claude".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reply_modes() {
        assert_eq!("manual".parse::<ReplyMode>().unwrap(), ReplyMode::Manual);
        assert_eq!("human".parse::<ReplyMode>().unwrap(), ReplyMode::Manual);
        assert_eq!("claude".parse::<ReplyMode>().unwrap(), ReplyMode::Claude);
        assert!("auto".parse::<ReplyMode>().is_err());
    }

    #[test]
    fn trims_radio_reply_to_frame_payload_limit() {
        let long = "x".repeat(modem::MAX_FRAME_PAYLOAD_BYTES + 100);
        let trimmed = trim_for_radio(&long);
        assert!(trimmed.len() <= modem::MAX_FRAME_PAYLOAD_BYTES - "VK2EMM: ".len());
    }

    #[test]
    fn request_uses_configured_model_and_limit() {
        let config = ClaudeConfig {
            api_key: "test-key".to_string(),
            model: "claude-fable-5".to_string(),
            max_tokens: 42,
            messages_url: DEFAULT_MESSAGES_URL.to_string(),
        };
        let request = build_request(&config, "Need weather at lat -33 lon 151");
        assert_eq!(request.model, "claude-fable-5");
        assert_eq!(request.max_tokens, 42);
        assert_eq!(request.messages[0].role, "user");
        assert!(request.system.contains("low bandwidth amateur radio"));
        assert!(request.system.contains("Field location hint"));
        assert_eq!(request.tools[0].kind, "web_search_20250305");
        assert_eq!(request.tools[0].max_uses, 3);
    }

    #[test]
    fn extracts_structured_location_payload() {
        let hint =
            extract_location_hint("VK2EMM/P: LOC:-33.868800,151.209300;ACC:12;MSG:weather here")
                .unwrap();
        assert!(hint.contains("lat -33.868800"));
        assert!(hint.contains("lon 151.209300"));
        assert!(hint.contains("accuracy 12m"));
    }

    #[test]
    fn detects_image_media_types() {
        assert_eq!(
            image_media_type("x.jpeg", &[0xFF, 0xD8, 0xFF, 0x00]).unwrap(),
            "image/jpeg"
        );
        assert_eq!(
            image_media_type("x.webp", b"RIFFxxxxWEBPmore").unwrap(),
            "image/webp"
        );
        assert!(image_media_type("x.bin", b"nope").is_err());
    }

    #[test]
    fn image_request_uses_multimodal_content() {
        let config = ClaudeConfig {
            api_key: "test-key".to_string(),
            model: "claude-fable-5".to_string(),
            max_tokens: 42,
            messages_url: DEFAULT_MESSAGES_URL.to_string(),
        };
        let request = build_image_request(
            &config,
            "describe".to_string(),
            "image/webp",
            "abc".to_string(),
        );
        match &request.messages[0].content {
            ClaudeMessageContent::Blocks(blocks) => assert_eq!(blocks.len(), 2),
            ClaudeMessageContent::Text(_) => panic!("image request should use content blocks"),
        }
    }
}
