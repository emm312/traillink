mod claude;
mod laptop_location;
mod sdr;
mod web;

use crate::claude::{ClaudeConfig, ReplyMode};
use crate::sdr::hackrf::{
    AudioDirection, LinkTelemetry, SdrCommand, SdrEvent, run_transceiver_loop,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use num_complex::Complex;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Bar, BarChart, BarGroup, Block, Borders, Gauge, List, ListItem, Paragraph},
};
use rustfft::FftPlanner;
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct App {
    input: String,
    messages: Vec<(String, MessageType)>,
    current_fft: Vec<f64>,
    waterfall_history: VecDeque<Vec<f64>>,
    is_tx: bool,
    audio_level: f64,
    image_status: Option<String>,
    reply_mode: ReplyMode,
    claude_config: Option<ClaudeConfig>,
    claude_pending: usize,
    last_location: Option<modem::location::Location>,
    active_sos: Option<SosAlert>,
    base_location: Option<modem::location::Location>,
    link: LinkDashboard,
}

struct LinkDashboard {
    snr_db: Option<f32>,
    rssi_dbm: Option<f32>,
    rssi_percent: f32,
    fec_corrections: usize,
    crc_pass: Option<bool>,
    sync_score: Option<f32>,
    last_error: Option<String>,
    burst_success: VecDeque<bool>,
    snr_history: VecDeque<f32>,
    rssi_history: VecDeque<f32>,
    pending_tx_at: Option<Instant>,
    round_trip: Option<Duration>,
}

impl Default for LinkDashboard {
    fn default() -> Self {
        Self {
            snr_db: None,
            rssi_dbm: None,
            rssi_percent: 0.0,
            fec_corrections: 0,
            crc_pass: None,
            sync_score: None,
            last_error: None,
            burst_success: VecDeque::new(),
            snr_history: VecDeque::new(),
            rssi_history: VecDeque::new(),
            pending_tx_at: None,
            round_trip: None,
        }
    }
}

#[derive(Clone)]
struct SosAlert {
    id: String,
    call: Option<String>,
    message: Option<String>,
    location: Option<modem::location::Location>,
    acknowledged: bool,
}

#[derive(Clone, Copy)]
enum MessageType {
    Rx,
    Tx,
    System,
}

enum ClaudeEvent {
    Response { prompt: String, reply: String },
    Error { prompt: String, error: String },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create channels for background SDR thread
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<SdrCommand>();
    let (msg_tx, msg_rx) = crossbeam_channel::unbounded::<SdrEvent>();
    let (audio_tx, audio_rx) = crossbeam_channel::unbounded();
    let (claude_tx, claude_rx) = crossbeam_channel::unbounded::<ClaudeEvent>();
    let (web_cmd_tx, web_cmd_rx) = crossbeam_channel::unbounded::<web::WebCommand>();
    let web_state = web::new_shared_state();

    // Shared VOX state
    let vox_active = Arc::new(AtomicBool::new(false));

    // Spawn SDR Transceiver background loop (Tuned to 434.0 MHz)
    let freq = 434_000_000;
    let sdr_cmd_tx = cmd_tx.clone();
    let sdr_msg_tx = msg_tx.clone();
    let vox_active_clone = vox_active.clone();
    std::thread::spawn(move || {
        if let Err(e) =
            run_transceiver_loop(freq, cmd_rx, sdr_msg_tx.clone(), vox_active_clone, audio_tx)
        {
            let _ = sdr_msg_tx.send(SdrEvent::Notice(format!(
                "System Error: SDR Transceiver failed: {}",
                e
            )));
        }
    });

    let mut startup_messages = vec![
        (
            "System: Welcome to TrailLink Base Station CLI Chat".to_string(),
            MessageType::System,
        ),
        (
            format!(
                "System: SDR initialized and listening on {:.3} MHz...",
                freq as f64 / 1_000_000.0
            ),
            MessageType::System,
        ),
    ];
    match web::start_server(web_state.clone(), web_cmd_tx) {
        Ok(addr) => startup_messages.push((
            format!("System: Base web dashboard listening on http://{}", addr),
            MessageType::System,
        )),
        Err(error) => startup_messages.push((
            format!("System: Base web dashboard disabled: {}", error),
            MessageType::System,
        )),
    }

    let mut reply_mode = match claude::mode_from_env() {
        Ok(mode) => mode,
        Err(e) => {
            startup_messages.push((
                format!("System: {}. Falling back to Manual mode.", e),
                MessageType::System,
            ));
            ReplyMode::Manual
        }
    };
    let claude_config = match ClaudeConfig::from_env() {
        Ok(config) => Some(config),
        Err(e) => {
            if reply_mode == ReplyMode::Claude {
                startup_messages.push((
                    format!("System: {}. Falling back to Manual mode.", e),
                    MessageType::System,
                ));
                reply_mode = ReplyMode::Manual;
            }
            None
        }
    };
    startup_messages.push((
        format!("System: Reply mode is {}", reply_mode.label()),
        MessageType::System,
    ));
    let (base_location, base_location_status) = load_base_location();
    if let Some(status) = base_location_status {
        startup_messages.push((status, MessageType::System));
    }

    let mut app = App {
        input: String::new(),
        messages: startup_messages,
        current_fft: vec![0.0; 107],
        waterfall_history: VecDeque::new(),
        is_tx: false,
        audio_level: 0.0,
        image_status: None,
        reply_mode,
        claude_config,
        claude_pending: 0,
        last_location: None,
        active_sos: None,
        base_location,
        link: LinkDashboard::default(),
    };

    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(1024);
    publish_web_snapshot(&web_state, &app, vox_active.load(Ordering::Relaxed), freq);

    loop {
        // Process incoming audio blocks for FFT (non-blocking)
        let mut got_audio = false;
        while let Ok(audio_block) = audio_rx.try_recv() {
            got_audio = true;
            let is_tx_audio = audio_block.direction == AudioDirection::Tx;
            app.is_tx = is_tx_audio;
            let samples = &audio_block.samples;

            // Calculate audio level (RMS)
            let mut sum_sq = 0.0f64;
            for &val in samples {
                sum_sq += (val as f64) * (val as f64);
            }
            let rms = (sum_sq / samples.len() as f64).sqrt();
            app.audio_level = ((rms / 0.5) * 100.0).min(100.0);

            let mut buffer: Vec<Complex<f32>> = samples
                .iter()
                .enumerate()
                .map(|(idx, &v)| {
                    // Apply Hann window to eliminate spectral leakage
                    let window = 0.5f32
                        * (1.0f32 - (2.0f32 * std::f32::consts::PI * idx as f32 / 1023.0f32).cos());
                    Complex::new(v * window, 0.0)
                })
                .collect();
            fft.process(&mut buffer);

            let mut magnitudes = vec![0.0f64; 107]; // Only keep frequencies up to 5 kHz (107 bins of 512)
            for i in 0..107 {
                let mag = (buffer[i].re.powi(2) + buffer[i].im.powi(2)).sqrt() as f64;
                // With Hann window, coherent gain is 0.5, so peak is A * N / 4 = A * 256.
                let normalized = mag / 256.0;
                let db = 20.0 * normalized.max(1e-5).log10();
                // 60 dB dynamic range, clamped to [0.0, 1.0] to prevent any clipping/overflow
                let visual_val = ((db + 60.0).max(0.0) / 60.0).min(1.0);
                magnitudes[i] = visual_val;
            }

            app.current_fft = magnitudes.clone();
            app.waterfall_history.push_front(magnitudes);
            if app.waterfall_history.len() > 50 {
                app.waterfall_history.pop_back();
            }
        }

        if !got_audio && !app.is_tx {
            app.audio_level = (app.audio_level * 0.8).max(0.0);
        }

        // Render TUI
        terminal.draw(|f| {
            let root_chunks = if app.active_sos.is_some() {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(1)])
                    .split(f.area())
            } else {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(0), Constraint::Min(1)])
                    .split(f.area())
            };

            if let Some(sos) = &app.active_sos {
                let ack = if sos.acknowledged { "ACKED" } else { "ACTIVE" };
                let loc = sos
                    .location
                    .map(format_location)
                    .unwrap_or_else(|| "no location".to_string());
                let banner = Paragraph::new(format!(
                    " SOS {} [{}] {} | A: ACK  E: CLEAR ",
                    sos.id, ack, loc
                ))
                .style(
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                )
                .block(Block::default().borders(Borders::ALL).title(" EMERGENCY "));
                f.render_widget(banner, root_chunks[0]);
            }

            let main_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(55), // Chat column
                    Constraint::Percentage(45), // Visualizer column
                ])
                .split(root_chunks[1]);

            let chat_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),    // Chat log
                    Constraint::Length(3), // Input bar
                    Constraint::Length(1), // Status/Help bar
                ])
                .split(main_chunks[0]);

            // 1. Render Chat Log
            let items: Vec<ListItem> = app
                .messages
                .iter()
                .map(|(text, msg_type)| {
                    let style = match msg_type {
                        MessageType::Rx => Style::default().fg(Color::Green),
                        MessageType::Tx => Style::default()
                            .fg(Color::LightBlue)
                            .add_modifier(Modifier::BOLD),
                        MessageType::System => Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    };
                    ListItem::new(Line::from(vec![Span::styled(text.clone(), style)]))
                })
                .collect();

            let chat_list = List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" TrailLink Chat History (Simplex) "),
            );
            f.render_widget(chat_list, chat_chunks[0]);

            // 2. Render Input field
            let input_widget = Paragraph::new(app.input.as_str()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Input (Press Enter to Transmit) "),
            );
            f.render_widget(input_widget, chat_chunks[1]);

            // 3. Render Status line
            let is_vox_active = vox_active.load(Ordering::Relaxed);
            let help_style = Style::default().fg(Color::DarkGray);

            let status_sub_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(9),  // TX/RX/IDLE Indicator
                    Constraint::Min(20),    // Mode, frequency, image transfer state
                    Constraint::Length(25), // RSSI Level Gauge
                ])
                .split(chat_chunks[2]);

            // Render TX/RX/IDLE Indicator
            let (state_str, state_color) = if app.is_tx {
                ("[  TX  ]", Color::Red)
            } else if is_vox_active {
                ("[  RX  ]", Color::Green)
            } else {
                ("[ IDLE ]", Color::DarkGray)
            };
            let indicator_widget = Paragraph::new(Span::styled(
                state_str,
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ));
            f.render_widget(indicator_widget, status_sub_chunks[0]);

            // Render Mode & Frequency info
            let mut status_spans = vec![
                Span::styled(" Mode: ", help_style),
                Span::styled(
                    app.reply_mode.label(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | Link: Simplex | Freq: ", help_style),
                Span::styled(
                    format!("{:.1} MHz", freq as f64 / 1_000_000.0),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | Tab: Toggle | Esc: Exit", help_style),
            ];
            if app.claude_pending > 0 {
                status_spans.push(Span::styled(" | Claude: ", help_style));
                status_spans.push(Span::styled(
                    app.claude_pending.to_string(),
                    Style::default().fg(Color::Yellow),
                ));
            }
            if let Some(image_status) = &app.image_status {
                status_spans.push(Span::styled(" | Image: ", help_style));
                status_spans.push(Span::styled(
                    image_status.clone(),
                    Style::default().fg(Color::LightBlue),
                ));
            }
            if let Some(location) = app.last_location {
                status_spans.push(Span::styled(" | Loc: ", help_style));
                status_spans.push(Span::styled(
                    format_location(location),
                    Style::default().fg(Color::LightGreen),
                ));
            }
            let status_line = Line::from(status_spans);
            let status_widget = Paragraph::new(status_line);
            f.render_widget(status_widget, status_sub_chunks[1]);

            // Render audio level gauge
            let gauge_color = if app.is_tx {
                Color::Red
            } else if app.audio_level < 30.0 {
                Color::Green
            } else if app.audio_level < 70.0 {
                Color::Yellow
            } else {
                Color::Red
            };

            let ratio = (app.audio_level / 100.0).clamp(0.0, 1.0);
            let gauge_label = if app.is_tx {
                format!("TX Audio {:.0}%", app.audio_level)
            } else {
                format!("RSSI {:.0}%", app.audio_level)
            };

            let gauge_widget = Gauge::default()
                .gauge_style(Style::default().fg(gauge_color).bg(Color::Rgb(30, 30, 30)))
                .ratio(ratio)
                .label(gauge_label);
            f.render_widget(gauge_widget, status_sub_chunks[2]);

            // 4. Render Visualizers Column
            let viz_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(16),     // Signal dashboard
                    Constraint::Percentage(40), // FFT Spectrum Chart
                    Constraint::Percentage(60), // Scrolling Waterfall History
                ])
                .split(main_chunks[1]);

            render_signal_dashboard(f, &app, viz_chunks[0]);

            let viz_width = viz_chunks[1].width.saturating_sub(2) as usize;

            // Render Spectrum Bar Chart
            let binned_fft = bin_fft(&app.current_fft, viz_width);
            let bars: Vec<Bar> = binned_fft
                .iter()
                .map(|&val| {
                    let height = (val * 100.0) as u64;
                    Bar::default().value(height).text_value("".into())
                })
                .collect();

            let spectrum_title = if app.is_tx {
                " TX Audio FFT Spectrum (0 - 5 kHz) "
            } else {
                " Audio FFT Spectrum (0 - 5 kHz) "
            };
            let barchart = BarChart::default()
                .block(Block::default().title(spectrum_title).borders(Borders::ALL))
                .bar_width(1)
                .bar_gap(0)
                .bar_style(Style::default().fg(Color::Cyan))
                .data(BarGroup::default().bars(&bars))
                .max(100);
            f.render_widget(barchart, viz_chunks[1]);

            // Render Waterfall History
            let waterfall_height = viz_chunks[2].height.saturating_sub(2) as usize;
            let waterfall_lines: Vec<Line> = app
                .waterfall_history
                .iter()
                .take(waterfall_height)
                .map(|slice| {
                    let binned_slice = bin_fft(slice, viz_width);
                    let spans: Vec<Span> = binned_slice
                        .iter()
                        .map(|&val| get_waterfall_span(val))
                        .collect();
                    Line::from(spans)
                })
                .collect();

            let waterfall_widget = Paragraph::new(waterfall_lines).block(
                Block::default()
                    .title(" Audio Waterfall History ")
                    .borders(Borders::ALL),
            );
            f.render_widget(waterfall_widget, viz_chunks[2]);
        })?;

        // Process incoming SDR messages (non-blocking)
        while let Ok(rx_event) = msg_rx.try_recv() {
            match rx_event {
                SdrEvent::Notice(rx_msg) => {
                    if let Some(cmd) = rx_msg.strip_prefix("AUTOTX:") {
                        // e.g. "REQ_CHUNKS id idx,idx..."
                        app.messages.push((
                            format!("System: Auto-requesting missing chunks: {}", cmd),
                            MessageType::System,
                        ));
                        let _ = sdr_cmd_tx.send(SdrCommand::Transmit(format!("VK2EMM: {}", cmd)));
                        note_tx(&mut app);
                    } else if let Some(progress) = rx_msg.strip_prefix("IMAGE_PROGRESS:") {
                        app.image_status = Some(progress.to_string());
                    } else if let Some(complete) = rx_msg.strip_prefix("IMAGE_COMPLETE:") {
                        app.image_status = Some("complete".to_string());
                        app.messages.push((
                            format!("System: Image received and saved to {}", complete),
                            MessageType::System,
                        ));
                        if let Some(config) = app.claude_config.clone() {
                            let image_path = complete.to_string();
                            let context = app.last_location.map(|location| {
                                format!("Latest field location: {}", format_location(location))
                            });
                            let tx = claude_tx.clone();
                            app.claude_pending += 1;
                            app.messages.push((
                                "System: Sending received image to Claude".to_string(),
                                MessageType::System,
                            ));
                            std::thread::spawn(move || {
                                let event = match claude::ask_claude_about_image(
                                    &config,
                                    &image_path,
                                    context.as_deref(),
                                ) {
                                    Ok(reply) => ClaudeEvent::Response {
                                        prompt: format!("image {}", image_path),
                                        reply,
                                    },
                                    Err(error) => ClaudeEvent::Error {
                                        prompt: format!("image {}", image_path),
                                        error,
                                    },
                                };
                                let _ = tx.send(event);
                            });
                        } else {
                            app.messages.push((
                                "System: Claude image analysis skipped; ANTHROPIC_API_KEY is not set"
                                    .to_string(),
                                MessageType::System,
                            ));
                        }
                    } else if let Some(error) = rx_msg.strip_prefix("IMAGE_ERROR:") {
                        app.image_status = Some("error".to_string());
                        app.messages
                            .push((format!("System Error: {}", error), MessageType::System));
                    } else {
                        app.messages.push((rx_msg, MessageType::System));
                    }
                }
                SdrEvent::Frame {
                    msg_type,
                    has_location,
                    payload,
                } => handle_rx_frame(&mut app, &claude_tx, msg_type, has_location, payload),
                SdrEvent::Telemetry(telemetry) => {
                    apply_link_telemetry(&mut app.link, telemetry);
                }
            }
        }

        while let Ok(event) = claude_rx.try_recv() {
            app.claude_pending = app.claude_pending.saturating_sub(1);
            match event {
                ClaudeEvent::Response { prompt, reply } => {
                    let trimmed = claude::trim_for_radio(&reply);
                    if trimmed.is_empty() {
                        app.messages.push((
                            format!("System: Claude returned an empty reply for: {}", prompt),
                            MessageType::System,
                        ));
                        continue;
                    }

                    let formatted = format!("VK2EMM: {}", trimmed);
                    app.messages
                        .push((format!("[TX/Claude] {}", trimmed), MessageType::Tx));
                    let _ = sdr_cmd_tx.send(SdrCommand::Transmit(formatted));
                    note_tx(&mut app);
                }
                ClaudeEvent::Error { prompt, error } => {
                    app.messages.push((
                        format!("System: Claude failed for '{}': {}", prompt, error),
                        MessageType::System,
                    ));
                }
            }
        }

        while let Ok(command) = web_cmd_rx.try_recv() {
            handle_web_command(&mut app, &sdr_cmd_tx, command);
        }

        // Handle user input
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
            && key.kind == event::KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break;
                }
                KeyCode::Esc => {
                    break;
                }
                KeyCode::Tab => {
                    let next_mode = app.reply_mode.toggle();
                    if next_mode == ReplyMode::Claude && app.claude_config.is_none() {
                        match ClaudeConfig::from_env() {
                            Ok(config) => {
                                app.claude_config = Some(config);
                                app.reply_mode = ReplyMode::Claude;
                                app.messages.push((
                                    "System: Reply mode switched to Claude".to_string(),
                                    MessageType::System,
                                ));
                            }
                            Err(e) => {
                                app.messages.push((
                                    format!("System: Cannot enable Claude mode: {}", e),
                                    MessageType::System,
                                ));
                            }
                        }
                    } else {
                        app.reply_mode = next_mode;
                        app.messages.push((
                            format!("System: Reply mode switched to {}", app.reply_mode.label()),
                            MessageType::System,
                        ));
                    }
                }
                KeyCode::Char(c)
                    if app.input.is_empty()
                        && matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'l') =>
                {
                    match c.to_ascii_lowercase() {
                        'a' => {
                            acknowledge_sos(&mut app, &sdr_cmd_tx);
                        }
                        'e' => {
                            clear_sos_banner(&mut app);
                        }
                        'l' => {
                            clear_field_location(&mut app);
                        }
                        _ => {}
                    }
                }
                KeyCode::Enter => {
                    let text = app.input.trim().to_string();
                    if !text.is_empty() {
                        transmit_text(&mut app, &sdr_cmd_tx, &text, "TX");
                        app.input.clear();
                    }
                }
                KeyCode::Char(c) => {
                    app.input.push(c);
                }
                KeyCode::Backspace => {
                    app.input.pop();
                }
                _ => {}
            }
        }

        publish_web_snapshot(&web_state, &app, vox_active.load(Ordering::Relaxed), freq);
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

fn note_tx(app: &mut App) {
    app.is_tx = true;
    app.audio_level = 0.0;
    app.link.pending_tx_at = Some(Instant::now());
}

fn handle_web_command(
    app: &mut App,
    sdr_cmd_tx: &crossbeam_channel::Sender<SdrCommand>,
    command: web::WebCommand,
) {
    match command {
        web::WebCommand::Transmit(text) => transmit_text(app, sdr_cmd_tx, &text, "TX/Web"),
        web::WebCommand::AckSos => acknowledge_sos(app, sdr_cmd_tx),
        web::WebCommand::ClearSos => clear_sos_banner(app),
        web::WebCommand::ClearLocation => clear_field_location(app),
    }
}

fn transmit_text(
    app: &mut App,
    sdr_cmd_tx: &crossbeam_channel::Sender<SdrCommand>,
    text: &str,
    label: &str,
) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }

    let formatted = if text.to_uppercase().starts_with("VK2EMM") {
        text.to_string()
    } else {
        format!("VK2EMM: {}", text)
    };
    let clean_local = strip_callsign(text);
    app.messages
        .push((format!("[{}] {}", label, clean_local), MessageType::Tx));
    let _ = sdr_cmd_tx.send(SdrCommand::Transmit(formatted));
    note_tx(app);
}

fn acknowledge_sos(app: &mut App, sdr_cmd_tx: &crossbeam_channel::Sender<SdrCommand>) {
    if let Some(sos) = &mut app.active_sos {
        let payload = format!("SOS_ACK:{}", sos.id);
        sos.acknowledged = true;
        app.messages.push((
            format!("System: Sending SOS acknowledgement for {}", sos.id),
            MessageType::System,
        ));
        let _ = sdr_cmd_tx.send(SdrCommand::TransmitFrame {
            msg_type: modem::frame::MsgType::Ack,
            has_location: false,
            payload,
        });
        note_tx(app);
    }
}

fn clear_sos_banner(app: &mut App) {
    app.active_sos = None;
    app.messages.push((
        "System: Emergency banner cleared".to_string(),
        MessageType::System,
    ));
}

fn clear_field_location(app: &mut App) {
    app.last_location = None;
    app.messages.push((
        "System: Last field location cleared".to_string(),
        MessageType::System,
    ));
}

fn publish_web_snapshot(
    state: &web::SharedWebState,
    app: &App,
    vox_active: bool,
    frequency_hz: u64,
) {
    if let Ok(mut snapshot) = state.write() {
        *snapshot = build_web_snapshot(app, vox_active, frequency_hz);
    }
}

fn build_web_snapshot(app: &App, vox_active: bool, frequency_hz: u64) -> web::BaseSnapshot {
    let message_start = app.messages.len().saturating_sub(120);
    let radio_state = if app.is_tx {
        "TX"
    } else if vox_active {
        "RX"
    } else {
        "IDLE"
    };

    web::BaseSnapshot {
        updated_at_ms: web::now_ms(),
        frequency_mhz: frequency_hz as f64 / 1_000_000.0,
        mode: app.reply_mode.label().to_string(),
        radio_state: radio_state.to_string(),
        audio_level: app.audio_level,
        image_status: app.image_status.clone(),
        claude_pending: app.claude_pending,
        messages: app.messages[message_start..]
            .iter()
            .map(|(text, kind)| web::WebMessage {
                text: text.clone(),
                kind: match kind {
                    MessageType::Rx => "rx",
                    MessageType::Tx => "tx",
                    MessageType::System => "system",
                }
                .to_string(),
            })
            .collect(),
        current_fft: app.current_fft.clone(),
        waterfall_history: app.waterfall_history.iter().cloned().collect(),
        base_location: app.base_location.map(web::LocationSnapshot::from),
        field_location: app.last_location.map(web::LocationSnapshot::from),
        active_sos: app.active_sos.as_ref().map(|sos| web::SosSnapshot {
            id: sos.id.clone(),
            call: sos.call.clone(),
            message: sos.message.clone(),
            location: sos.location.map(web::LocationSnapshot::from),
            acknowledged: sos.acknowledged,
        }),
        link: web::LinkSnapshot {
            snr_db: app.link.snr_db,
            rssi_dbm: app.link.rssi_dbm,
            rssi_percent: app.link.rssi_percent,
            fec_corrections: app.link.fec_corrections,
            crc_pass: app.link.crc_pass,
            sync_score: app.link.sync_score,
            packet_loss_percent: packet_loss_percent(&app.link),
            round_trip_secs: app.link.round_trip.map(|duration| duration.as_secs_f64()),
            last_error: app.link.last_error.clone(),
            snr_history: app.link.snr_history.iter().copied().collect(),
            rssi_history: app.link.rssi_history.iter().copied().collect(),
            burst_success: app.link.burst_success.iter().copied().collect(),
        },
    }
}

fn load_base_location() -> (Option<modem::location::Location>, Option<String>) {
    if let (Ok(lat), Ok(lon)) = (
        std::env::var("TRAILLINK_BASE_LAT"),
        std::env::var("TRAILLINK_BASE_LON"),
    ) {
        let Ok(lat) = lat.parse() else {
            return (
                None,
                Some("System: Invalid TRAILLINK_BASE_LAT; base map distance disabled".to_string()),
            );
        };
        let Ok(lon) = lon.parse() else {
            return (
                None,
                Some("System: Invalid TRAILLINK_BASE_LON; base map distance disabled".to_string()),
            );
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            return (
                None,
                Some(
                    "System: Base location env coordinates out of range; map distance disabled"
                        .to_string(),
                ),
            );
        }
        let location = modem::location::Location {
            lat,
            lon,
            accuracy_m: None,
        };
        return (
            Some(location),
            Some(format!(
                "System: Base location from env: {}",
                format_location(location)
            )),
        );
    }

    if matches!(
        std::env::var("TRAILLINK_BASE_AUTO_LOCATION").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE") | Ok("off") | Ok("OFF")
    ) {
        return (None, None);
    }

    match laptop_location::current_location(Duration::from_secs(4)) {
        Ok(location) => (
            Some(location),
            Some(format!(
                "System: Base location from laptop: {}",
                format_location(location)
            )),
        ),
        Err(error) => (
            None,
            Some(format!(
                "System: Laptop location unavailable: {}. Set TRAILLINK_BASE_LAT/LON to enable map distance.",
                error
            )),
        ),
    }
}

fn apply_link_telemetry(link: &mut LinkDashboard, telemetry: LinkTelemetry) {
    if let Some(snr_db) = telemetry.snr_db {
        link.snr_db = Some(snr_db);
        push_bounded(&mut link.snr_history, snr_db, 30);
    }
    if let Some(rssi_dbm) = telemetry.rssi_dbm {
        link.rssi_dbm = Some(rssi_dbm);
    }
    link.rssi_percent = telemetry.rssi_percent;
    push_bounded(&mut link.rssi_history, telemetry.rssi_percent, 30);
    link.fec_corrections = telemetry.fec_corrections;
    if telemetry.crc_pass.is_some() {
        link.crc_pass = telemetry.crc_pass;
    }
    link.sync_score = telemetry.sync_score;
    link.last_error = telemetry.last_error;

    if telemetry.decoded_frames > 0 {
        push_bounded(&mut link.burst_success, true, 40);
    } else if telemetry.failed_frames > 0 {
        push_bounded(&mut link.burst_success, false, 40);
    }
}

fn push_bounded<T>(values: &mut VecDeque<T>, value: T, max_len: usize) {
    values.push_back(value);
    while values.len() > max_len {
        values.pop_front();
    }
}

fn packet_loss_percent(link: &LinkDashboard) -> Option<f64> {
    if link.burst_success.is_empty() {
        return None;
    }
    let failed = link
        .burst_success
        .iter()
        .filter(|success| !**success)
        .count();
    Some((failed as f64 / link.burst_success.len() as f64) * 100.0)
}

fn render_signal_dashboard(f: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .title(" TRAILLINK BASE ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(inner);

    let crc = match app.link.crc_pass {
        Some(true) => "PASS".to_string(),
        Some(false) => "FAIL".to_string(),
        None => "--".to_string(),
    };
    let gps = if app.last_location.is_some() {
        "LOCKED"
    } else {
        "NO FIX"
    };
    let packet_loss = packet_loss_percent(&app.link)
        .map(|loss| format!("{loss:.0}%"))
        .unwrap_or_else(|| "--".to_string());
    let round_trip = app
        .link
        .round_trip
        .map(format_duration_secs)
        .unwrap_or_else(|| "--".to_string());
    let snr = app
        .link
        .snr_db
        .map(|value| format!("{value:.1} dB"))
        .unwrap_or_else(|| "--".to_string());
    let rssi = app
        .link
        .rssi_dbm
        .map(|value| format!("{value:.0} dBm"))
        .unwrap_or_else(|| "--".to_string());
    let sync = app
        .link
        .sync_score
        .map(|value| format!("{value:.1}/16"))
        .unwrap_or_else(|| "--".to_string());
    let image = app.image_status.as_deref().unwrap_or("--");

    let metric_lines = vec![
        Line::from(vec![
            Span::styled("SNR: ", Style::default().fg(Color::DarkGray)),
            Span::styled(snr, status_style(app.link.snr_db, 6.0, 10.0)),
        ]),
        Line::from(vec![
            Span::styled("RSSI: ", Style::default().fg(Color::DarkGray)),
            Span::styled(rssi, Style::default().fg(Color::LightBlue)),
        ]),
        Line::from(vec![
            Span::styled("Packet loss: ", Style::default().fg(Color::DarkGray)),
            Span::styled(packet_loss, loss_style(packet_loss_percent(&app.link))),
        ]),
        Line::from(vec![
            Span::styled("FEC corrections: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.link.fec_corrections.to_string(),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("CRC: ", Style::default().fg(Color::DarkGray)),
            Span::styled(crc, crc_style(app.link.crc_pass)),
        ]),
        Line::from(vec![
            Span::styled("GPS: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                gps,
                if app.last_location.is_some() {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("Round trip: ", Style::default().fg(Color::DarkGray)),
            Span::styled(round_trip, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Sync: ", Style::default().fg(Color::DarkGray)),
            Span::styled(sync, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled("Image: ", Style::default().fg(Color::DarkGray)),
            Span::styled(image.to_string(), image_style(app.image_status.as_deref())),
        ]),
        Line::from(vec![
            Span::styled("SNR ", Style::default().fg(Color::DarkGray)),
            Span::raw(history_bar(&app.link.snr_history, 0.0, 20.0, 18)),
        ]),
        Line::from(vec![
            Span::styled("RSSI ", Style::default().fg(Color::DarkGray)),
            Span::raw(history_bar(&app.link.rssi_history, 0.0, 100.0, 18)),
        ]),
    ];
    f.render_widget(Paragraph::new(metric_lines), chunks[0]);

    let map_lines = build_mini_map(app.base_location, app.last_location, chunks[1].width, 10);
    f.render_widget(Paragraph::new(map_lines), chunks[1]);
}

fn status_style(value: Option<f32>, warn: f32, good: f32) -> Style {
    match value {
        Some(value) if value >= good => Style::default().fg(Color::Green),
        Some(value) if value >= warn => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Red),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn crc_style(value: Option<bool>) -> Style {
    match value {
        Some(true) => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        Some(false) => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn loss_style(value: Option<f64>) -> Style {
    match value {
        Some(value) if value <= 5.0 => Style::default().fg(Color::Green),
        Some(value) if value <= 20.0 => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Red),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn image_style(value: Option<&str>) -> Style {
    match value {
        Some("complete") => Style::default().fg(Color::Green),
        Some("error") => Style::default().fg(Color::Red),
        Some(_) => Style::default().fg(Color::LightBlue),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn history_bar(values: &VecDeque<f32>, min: f32, max: f32, width: usize) -> String {
    const LEVELS: [char; 8] = ['_', '.', ':', '-', '=', '+', '*', '#'];
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let skip = values.len().saturating_sub(width);
    for value in values.iter().skip(skip) {
        let ratio = ((*value - min) / (max - min)).clamp(0.0, 1.0);
        let idx = (ratio * (LEVELS.len() - 1) as f32).round() as usize;
        out.push(LEVELS[idx]);
    }
    while out.len() < width {
        out.insert(0, ' ');
    }
    out
}

fn build_mini_map(
    base: Option<modem::location::Location>,
    field: Option<modem::location::Location>,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let map_width = usize::from(width.saturating_sub(1)).clamp(12, 40);
    let map_height = usize::from(height).clamp(6, 10);
    let mut grid = vec![vec!['.'; map_width]; map_height];
    let center_x = map_width / 2;
    let center_y = map_height / 2;
    grid[center_y][center_x] = 'B';

    if let Some(field) = field {
        let (field_x, field_y) = if let Some(base) = base {
            project_location(base, field, map_width, map_height)
        } else {
            (
                (center_x + 2).min(map_width.saturating_sub(1)),
                center_y.saturating_sub(1),
            )
        };
        grid[field_y][field_x] = 'F';
    }

    let mut lines: Vec<Line> = grid
        .into_iter()
        .map(|row| {
            Line::from(
                row.into_iter()
                    .map(|cell| match cell {
                        'B' => Span::styled("B", Style::default().fg(Color::Cyan)),
                        'F' => Span::styled("F", Style::default().fg(Color::Green)),
                        _ => Span::styled(".", Style::default().fg(Color::DarkGray)),
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();

    let footer = match (base, field) {
        (Some(base), Some(field)) => {
            let (distance_m, bearing_deg) = distance_and_bearing(base, field);
            format!(
                "F {:.1}km {:03.0}deg",
                distance_m / 1000.0,
                bearing_deg.round()
            )
        }
        (None, Some(_)) => "F seen; base coords unset".to_string(),
        (_, None) => "waiting for field GPS".to_string(),
    };
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().fg(Color::DarkGray),
    )));
    lines
}

fn project_location(
    base: modem::location::Location,
    field: modem::location::Location,
    width: usize,
    height: usize,
) -> (usize, usize) {
    let avg_lat = ((base.lat + field.lat) / 2.0).to_radians();
    let east_km = (field.lon - base.lon) * 111.320 * avg_lat.cos();
    let north_km = (field.lat - base.lat) * 110.574;
    let range_km = east_km.abs().max(north_km.abs()).max(0.25) * 1.2;
    let half_width = (width.saturating_sub(1) as f64) / 2.0;
    let half_height = (height.saturating_sub(1) as f64) / 2.0;
    let x = (half_width + (east_km / range_km) * half_width)
        .round()
        .clamp(0.0, width.saturating_sub(1) as f64) as usize;
    let y = (half_height - (north_km / range_km) * half_height)
        .round()
        .clamp(0.0, height.saturating_sub(1) as f64) as usize;
    (x, y)
}

fn distance_and_bearing(
    base: modem::location::Location,
    field: modem::location::Location,
) -> (f64, f64) {
    let lat1 = base.lat.to_radians();
    let lat2 = field.lat.to_radians();
    let dlat = (field.lat - base.lat).to_radians();
    let dlon = (field.lon - base.lon).to_radians();

    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    let distance_m = 6_371_000.0 * 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    let bearing = (y.atan2(x).to_degrees() + 360.0) % 360.0;

    (distance_m, bearing)
}

fn format_duration_secs(duration: Duration) -> String {
    format!("{:.1}s", duration.as_secs_f64())
}

fn strip_callsign(text: &str) -> &str {
    if let Some(stripped) = text.strip_prefix("VK2EMM/P: ") {
        stripped
    } else if let Some(stripped) = text.strip_prefix("VK2EMM: ") {
        stripped
    } else if text.starts_with("VK2EMM/") {
        if let Some(pos) = text.find(": ") {
            if text[..pos].starts_with("VK2EMM") {
                &text[pos + 2..]
            } else {
                text
            }
        } else {
            text
        }
    } else {
        text
    }
}

fn is_field_text_message(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("IMAGE_")
        || trimmed.starts_with("AUTOTX:")
        || trimmed.contains("REQ_CHUNKS")
    {
        return false;
    }
    if trimmed.starts_with("VK2EMM: ") {
        return false;
    }
    true
}

fn handle_rx_frame(
    app: &mut App,
    claude_tx: &crossbeam_channel::Sender<ClaudeEvent>,
    msg_type: modem::frame::MsgType,
    has_location: bool,
    payload: String,
) {
    if let Some(sent_at) = app.link.pending_tx_at.take() {
        app.link.round_trip = Some(sent_at.elapsed());
    }

    if has_location
        && let Some(location_message) = modem::location::parse_location_message(&payload)
    {
        app.last_location = Some(location_message.location);
    }

    if msg_type == modem::frame::MsgType::SOS
        && let Some(sos) = parse_sos_payload(&payload)
    {
        app.last_location = sos.location.or(app.last_location);
        app.messages.push((
            format!(
                "[SOS] {} {}",
                sos.call.clone().unwrap_or_else(|| "unknown".to_string()),
                sos.message
                    .clone()
                    .unwrap_or_else(|| "Emergency".to_string())
            ),
            MessageType::Rx,
        ));
        app.active_sos = Some(sos);
        return;
    }

    let clean_rx = strip_callsign(&payload).to_string();
    app.messages
        .push((format!("[RX] {}", clean_rx), MessageType::Rx));

    if app.reply_mode == ReplyMode::Claude && is_field_text_message(&payload) {
        if let Some(config) = app.claude_config.clone() {
            let prompt = clean_rx.clone();
            let tx = claude_tx.clone();
            app.claude_pending += 1;
            app.messages.push((
                format!("System: Asking Claude about: {}", prompt),
                MessageType::System,
            ));
            std::thread::spawn(move || {
                let event = match claude::ask_claude(&config, &prompt) {
                    Ok(reply) => ClaudeEvent::Response { prompt, reply },
                    Err(error) => ClaudeEvent::Error { prompt, error },
                };
                let _ = tx.send(event);
            });
        } else {
            app.messages.push((
                "System: Claude mode has no ANTHROPIC_API_KEY; staying receive-only.".to_string(),
                MessageType::System,
            ));
        }
    }
}

fn parse_sos_payload(payload: &str) -> Option<SosAlert> {
    let sos_start = payload.find("SOS:")? + "SOS:".len();
    let after_sos = &payload[sos_start..];
    let id_end = after_sos.find(';').unwrap_or(after_sos.len());
    let id = after_sos[..id_end].trim();
    if id.is_empty() {
        return None;
    }

    let mut call = None;
    let mut message = None;
    for part in after_sos[id_end..].split(';') {
        if let Some(value) = part.strip_prefix("CALL:") {
            let value = value.trim();
            if !value.is_empty() {
                call = Some(value.to_string());
            }
        } else if let Some(value) = part.strip_prefix("MSG:") {
            let value = value.trim();
            if !value.is_empty() {
                message = Some(value.to_string());
            }
        }
    }

    let location = modem::location::parse_location_message(payload).map(|parsed| parsed.location);
    Some(SosAlert {
        id: id.to_string(),
        call,
        message,
        location,
        acknowledged: false,
    })
}

fn format_location(location: modem::location::Location) -> String {
    match location.accuracy_m {
        Some(acc) => format!("{:.6},{:.6} +/- {:.0}m", location.lat, location.lon, acc),
        None => format!("{:.6},{:.6}", location.lat, location.lon),
    }
}

fn bin_fft(magnitudes: &[f64], target_width: usize) -> Vec<f64> {
    if target_width == 0 {
        return Vec::new();
    }
    let mut binned = vec![0.0; target_width];
    let chunk_size = magnitudes.len() as f64 / target_width as f64;
    for (i, slot) in binned.iter_mut().enumerate().take(target_width) {
        let start_idx = (i as f64 * chunk_size).floor() as usize;
        let end_idx = (((i + 1) as f64 * chunk_size).floor() as usize).min(magnitudes.len());
        if end_idx > start_idx {
            let sum: f64 = magnitudes[start_idx..end_idx].iter().sum();
            *slot = sum / (end_idx - start_idx) as f64;
        }
    }
    binned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_text_filter_skips_control_and_base_replies() {
        assert!(is_field_text_message("VK2EMM/P: need help"));
        assert!(is_field_text_message(
            "VK2EMM/P: LOC:-33.868800,151.209300;ACC:10;MSG:need help"
        ));
        assert!(is_field_text_message("need help"));
        assert!(!is_field_text_message("VK2EMM: base reply"));
        assert!(!is_field_text_message("IMAGE_PROGRESS:1 2/3"));
        assert!(!is_field_text_message("AUTOTX:REQ_CHUNKS 1 2"));
        assert!(!is_field_text_message("VK2EMM/P: REQ_CHUNKS 1 2"));
    }

    #[test]
    fn parses_sos_payload() {
        let sos = parse_sos_payload(
            "SOS:123;CALL:VK2EMM/P;LOC:-33.868800,151.209300;ACC:12;MSG:broken ankle",
        )
        .unwrap();
        assert_eq!(sos.id, "123");
        assert_eq!(sos.call.as_deref(), Some("VK2EMM/P"));
        assert_eq!(sos.message.as_deref(), Some("broken ankle"));
        assert_eq!(sos.location.unwrap().lon, 151.2093);
    }

    #[test]
    fn telemetry_updates_rolling_packet_loss() {
        let mut link = LinkDashboard::default();
        apply_link_telemetry(
            &mut link,
            LinkTelemetry {
                snr_db: Some(8.4),
                rssi_dbm: Some(-91.0),
                rssi_percent: 36.0,
                fec_corrections: 3,
                crc_pass: Some(true),
                decoded_frames: 1,
                failed_frames: 0,
                sync_score: Some(12.5),
                last_error: None,
            },
        );
        apply_link_telemetry(
            &mut link,
            LinkTelemetry {
                snr_db: None,
                rssi_dbm: Some(-105.0),
                rssi_percent: 12.0,
                fec_corrections: 0,
                crc_pass: Some(false),
                decoded_frames: 0,
                failed_frames: 1,
                sync_score: Some(9.7),
                last_error: Some("CRC check failed".to_string()),
            },
        );

        assert_eq!(packet_loss_percent(&link), Some(50.0));
        assert_eq!(link.snr_db, Some(8.4));
        assert_eq!(link.fec_corrections, 0);
        assert_eq!(link.crc_pass, Some(false));
        assert_eq!(link.last_error.as_deref(), Some("CRC check failed"));
    }

    #[test]
    fn map_projection_places_north_east_field_above_and_right() {
        let base = test_location(-33.8688, 151.2093);
        let field = test_location(-33.8588, 151.2193);
        let (x, y) = project_location(base, field, 21, 9);

        assert!(x > 10);
        assert!(y < 4);
    }

    #[test]
    fn distance_and_bearing_for_due_north() {
        let base = test_location(0.0, 0.0);
        let field = test_location(0.01, 0.0);
        let (distance_m, bearing_deg) = distance_and_bearing(base, field);

        assert!((distance_m - 1111.9).abs() < 10.0);
        assert!(bearing_deg < 1.0 || bearing_deg > 359.0);
    }

    fn test_location(lat: f64, lon: f64) -> modem::location::Location {
        modem::location::Location {
            lat,
            lon,
            accuracy_m: None,
        }
    }
}

fn get_waterfall_span(val: f64) -> Span<'static> {
    if val < 0.1 {
        Span::styled(" ", Style::default())
    } else if val < 0.25 {
        Span::styled("░", Style::default().fg(Color::Blue))
    } else if val < 0.4 {
        Span::styled("▒", Style::default().fg(Color::Blue))
    } else if val < 0.55 {
        Span::styled("▒", Style::default().fg(Color::Cyan))
    } else if val < 0.7 {
        Span::styled("▓", Style::default().fg(Color::Green))
    } else if val < 0.85 {
        Span::styled("▓", Style::default().fg(Color::Yellow))
    } else {
        Span::styled("█", Style::default().fg(Color::Red))
    }
}
