mod audio;
mod server;
mod state;

use state::AppState;
use std::net::SocketAddr;

fn parse_args_and_env(args: &[String]) -> Result<(String, u16, String, String), String> {
    let mut host = std::env::var("FIELD_HOST")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "::".to_string());
    let mut port_str = std::env::var("FIELD_PORT")
        .or_else(|_| std::env::var("PORT"))
        .unwrap_or_else(|_| "8080".to_string());
    let mut cert_path = std::env::var("FIELD_CERT")
        .or_else(|_| std::env::var("CERT"))
        .unwrap_or_else(|_| "cert.pem".to_string());
    let mut key_path = std::env::var("FIELD_KEY")
        .or_else(|_| std::env::var("KEY"))
        .unwrap_or_else(|_| "key.pem".to_string());

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" | "-h" => {
                if i + 1 < args.len() {
                    host = args[i + 1].clone();
                    i += 2;
                } else {
                    return Err("Error: --host requires an argument".to_string());
                }
            }
            "--port" | "-p" => {
                if i + 1 < args.len() {
                    port_str = args[i + 1].clone();
                    i += 2;
                } else {
                    return Err("Error: --port requires an argument".to_string());
                }
            }
            "--cert" | "-c" => {
                if i + 1 < args.len() {
                    cert_path = args[i + 1].clone();
                    i += 2;
                } else {
                    return Err("Error: --cert requires an argument".to_string());
                }
            }
            "--key" | "-k" => {
                if i + 1 < args.len() {
                    key_path = args[i + 1].clone();
                    i += 2;
                } else {
                    return Err("Error: --key requires an argument".to_string());
                }
            }
            _ => {
                return Err(format!("Unknown argument: {}", args[i]));
            }
        }
    }

    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("Error: Invalid port number: {}", port_str))?;

    // Validate that the host and port can be parsed into a SocketAddr
    let addr_str = if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    };
    let _: SocketAddr = addr_str
        .parse()
        .map_err(|_| format!("Error: Invalid host or port: {}:{}", host, port))?;

    Ok((host, port, cert_path, key_path))
}

fn ensure_certs(cert_path: &str, key_path: &str) -> Result<(), String> {
    use std::path::Path;
    let cert_exists = Path::new(cert_path).exists();
    let key_exists = Path::new(key_path).exists();

    if cert_exists && key_exists {
        return Ok(());
    }

    println!(
        "SSL certificates not found (cert: {}, key: {}). Attempting auto-generation using openssl...",
        cert_path, key_path
    );

    let hostname = std::process::Command::new("hostname")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|_| "raspberrypi".to_string());

    let san_ext = format!(
        "subjectAltName = DNS:traillink.local, DNS:localhost, DNS:{}, DNS:{}.local, IP:127.0.0.1",
        hostname, hostname
    );

    let output = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-keyout",
            key_path,
            "-out",
            cert_path,
            "-sha256",
            "-days",
            "3650",
            "-nodes",
            "-subj",
            "/CN=traillink.local",
            "-addext",
            &san_ext,
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            println!(
                "Successfully generated self-signed SSL certificates with SANs: {}",
                san_ext
            );
            Ok(())
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(format!("openssl execution failed: {}", err))
        }
        Err(e) => Err(format!(
            "Failed to run openssl: {}. Please generate self-signed certificates manually and place them at {} and {}.",
            e, cert_path, key_path
        )),
    }
}

#[tokio::main]
async fn main() {
    println!("==================================================");
    println!("       Blackbird Raspberry Pi Field Station        ");
    println!("==================================================");

    // 1. Initialize shared AppState
    let state = AppState::new();

    // 2. Spawn arecord capture and gate evaluation loops
    audio::spawn_audio_loops(state.clone());

    // 3. Create Web API router
    let app = server::make_router(state);

    // 4. Parse host and port from environment variables or command-line arguments
    let args: Vec<String> = std::env::args().collect();
    let (host, port, cert_path, key_path) = match parse_args_and_env(&args) {
        Ok(res) => res,
        Err(e) => {
            eprintln!("{}", e);
            eprintln!(
                "Usage: {} [--host <ip>] [--port <port>] [--cert <path>] [--key <path>]",
                args.first().cloned().unwrap_or_default()
            );
            std::process::exit(1);
        }
    };

    let addr_str = if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    };
    let addr: SocketAddr = addr_str.parse().unwrap();

    // 5. Load/Configure SSL
    if let Err(e) = ensure_certs(&cert_path, &key_path) {
        eprintln!("Fatal TLS Error: {}", e);
        std::process::exit(1);
    }

    let config = match axum_server::tls_rustls::RustlsConfig::from_pem_file(
        std::path::PathBuf::from(&cert_path),
        std::path::PathBuf::from(&key_path),
    )
    .await
    {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Fatal Error: Failed to load TLS certificates: {}", e);
            std::process::exit(1);
        }
    };

    println!("Web Control Panel API listening on https://{}", addr);

    if let Err(e) = axum_server::bind_rustls(addr, config)
        .serve(app.into_make_service())
        .await
    {
        eprintln!("Fatal Error: Axum HTTPS server crashed: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_app_state_broadcast() {
        let state = AppState::new();
        let mut rx = state.subscribe();

        state.add_message("Hello from test".to_string()).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received, "Hello from test");

        let status = state.get_status().await;
        assert_eq!(status.rx_messages.last().unwrap(), "Hello from test");
    }

    #[test]
    fn test_parse_args_basic() {
        let args = vec![
            "target/debug/field".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            "9090".to_string(),
            "--cert".to_string(),
            "mycert.pem".to_string(),
            "--key".to_string(),
            "mykey.pem".to_string(),
        ];
        let (host, port, cert, key) = parse_args_and_env(&args).unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 9090);
        assert_eq!(cert, "mycert.pem");
        assert_eq!(key, "mykey.pem");
    }

    #[test]
    fn test_parse_args_shorthand() {
        let args = vec![
            "target/debug/field".to_string(),
            "-h".to_string(),
            "10.0.0.1".to_string(),
            "-p".to_string(),
            "1234".to_string(),
            "-c".to_string(),
            "test_cert.pem".to_string(),
            "-k".to_string(),
            "test_key.pem".to_string(),
        ];
        let (host, port, cert, key) = parse_args_and_env(&args).unwrap();
        assert_eq!(host, "10.0.0.1");
        assert_eq!(port, 1234);
        assert_eq!(cert, "test_cert.pem");
        assert_eq!(key, "test_key.pem");
    }

    #[test]
    fn test_parse_args_invalid_port() {
        let args = vec![
            "target/debug/field".to_string(),
            "-p".to_string(),
            "abc".to_string(),
        ];
        let res = parse_args_and_env(&args);
        assert!(res.is_err());
    }

    #[test]
    fn test_parse_args_invalid_host() {
        let args = vec![
            "target/debug/field".to_string(),
            "-h".to_string(),
            "invalid_ip".to_string(),
        ];
        let res = parse_args_and_env(&args);
        assert!(res.is_err());
    }

    #[test]
    fn test_parse_args_unknown() {
        let args = vec!["target/debug/field".to_string(), "--foo".to_string()];
        let res = parse_args_and_env(&args);
        assert!(res.is_err());
    }

    #[test]
    fn test_parse_args_defaults() {
        let args = vec!["target/debug/field".to_string()];
        let (host, port, cert, key) = parse_args_and_env(&args).unwrap();
        assert_eq!(host, "::");
        assert_eq!(port, 8080);
        assert_eq!(cert, "cert.pem");
        assert_eq!(key, "key.pem");
    }

    #[tokio::test]
    async fn test_image_chunking_and_reassembly() {
        let state = AppState::new();
        let mut rx = state.subscribe();

        // 1. Create original mock image data
        let original_data: Vec<u8> = (0..100).map(|x| x as u8).collect();
        let expected_base64 = crate::state::base64_encode(&original_data);

        // 2. Fragment into 3 chunks
        let chunk_size = 35; // 3 chunks: 35, 35, 30 bytes
        let total_chunks = 3;
        let image_id = 42u32;

        for chunk_idx in 0..total_chunks {
            let start = chunk_idx * chunk_size;
            let end = std::cmp::min((chunk_idx + 1) * chunk_size, original_data.len());
            let chunk_data = &original_data[start..end];

            let payload = modem::image::encode_image_chunk_payload(&modem::image::ImageChunk {
                image_id,
                chunk_idx: chunk_idx as u16,
                total_chunks: total_chunks as u16,
                data: chunk_data.to_vec(),
            })
            .unwrap();

            state.handle_image_chunk(&payload).await;
        }

        // 3. Receive reassembled message from channel
        let msg = rx.recv().await.unwrap();
        assert!(msg.starts_with("IMAGE:data:image/jpeg;base64,"));
        let actual_base64 = msg.strip_prefix("IMAGE:data:image/jpeg;base64,").unwrap();
        assert_eq!(actual_base64, expected_base64);

        // 4. Verify base64 decode yields the original data
        let decoded_data = crate::state::base64_decode(actual_base64).unwrap();
        assert_eq!(decoded_data, original_data);
    }

    #[tokio::test]
    async fn test_image_buffer_restarts_on_total_chunk_mismatch() {
        let state = AppState::new();

        let stale_payload = modem::image::encode_image_chunk_payload(&modem::image::ImageChunk {
            image_id: 7,
            chunk_idx: 0,
            total_chunks: 3,
            data: vec![1, 2, 3],
        })
        .unwrap();
        state.handle_image_chunk(&stale_payload).await;

        let replacement_payload =
            modem::image::encode_image_chunk_payload(&modem::image::ImageChunk {
                image_id: 7,
                chunk_idx: 0,
                total_chunks: 2,
                data: vec![4, 5, 6],
            })
            .unwrap();
        state.handle_image_chunk(&replacement_payload).await;

        let inner = state.inner.read().await;
        let buffer = inner.image_buffers.get(&7).unwrap();
        assert_eq!(buffer.total_chunks, 2);
        assert_eq!(buffer.chunks[0], Some(vec![4, 5, 6]));
    }

    #[tokio::test]
    async fn test_invalid_image_chunk_does_not_mutate_buffers() {
        let state = AppState::new();
        state.handle_image_chunk(&[0, 1, 2]).await;

        let inner = state.inner.read().await;
        assert!(inner.image_buffers.is_empty());
    }

    #[test]
    fn test_base64_decode_rejects_invalid_input() {
        assert!(crate::state::base64_decode("not valid !!!").is_err());
    }
}
