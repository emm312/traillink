# TrailLink

Off-grid AI assistance over a custom radio link. A phone joins the field
station's local WiFi, sends a message through a web chat UI, and the field
station transmits it over an FM handheld using a custom 4-FSK audio modem. The
base station receives the message, can optionally ask Claude for a terse reply,
and sends the response back over the same link.

The project was built for a hackathon, so the design favors reliability and
simple operations.

A full technical writeup can be found here: https://www.emmanuelk.com.au/blog/traillink-amateur-radio-data-link

## Workspace

This is a Rust workspace with three crates:

- `modem` contains the framing, CRC, FEC, modulation, demodulation, image
  chunking, and location payload code. It has no async runtime or hardware I/O.
- `field` runs on the Raspberry Pi field station or laptop fallback. It serves
  the HTTPS chat UI, manages field-side state, receives web commands, and drives
  audio transmission.
- `base` runs on the base station laptop. It manages HackRF RX/TX, the terminal
  UI, the base web dashboard, and optional Claude replies.

## Radio Protocol

TrailLink uses a public, unencrypted 4-FSK protocol in the audio passband:

- tones: 600, 1200, 1800, and 2400 Hz
- symbol rate: 600 symbols/s, 2 bits per symbol
- sample rate: 48 kHz
- sync-based frame detection with CRC-16/CCITT-FALSE blocks
- optional Hamming(7,4) FEC
- message types for queries, responses, acknowledgements, broadcasts, SOS, and
  image chunks

## Development

Build the full workspace:

```bash
cargo build
```

Run tests:

```bash
cargo test
```

Run the field station locally:

```bash
cargo run -p field
```

Run the base station locally:

```bash
cargo run -p base
```

## Field Station

The field station serves the chat UI over HTTPS. By default it listens on all
interfaces at port `8080` and uses `cert.pem` / `key.pem`. If the certificate
files are missing, it attempts to generate self-signed certificates with
`openssl`.

Runtime options can be passed with environment variables:

```bash
FIELD_HOST=0.0.0.0 FIELD_PORT=8080 FIELD_CERT=cert.pem FIELD_KEY=key.pem cargo run -p field
```

Or with CLI flags:

```bash
cargo run -p field -- --host 0.0.0.0 --port 8080 --cert cert.pem --key key.pem
```

Supported field options:

- `FIELD_HOST` or `HOST`
- `FIELD_PORT` or `PORT`
- `FIELD_CERT` or `CERT`
- `FIELD_KEY` or `KEY`

## Base Station

The base station starts a terminal UI and initializes the SDR transceiver path.
It also starts a local web dashboard when available.

Claude replies are optional. Manual reply mode is the default. To enable Claude
mode, set `BASE_REPLY_MODE=claude` and provide an Anthropic API key:

```bash
BASE_REPLY_MODE=claude ANTHROPIC_API_KEY=... cargo run -p base
```

Supported Claude-related environment variables:

- `BASE_REPLY_MODE`: `manual` or `claude`
- `ANTHROPIC_API_KEY`
- `ANTHROPIC_MODEL`
- `ANTHROPIC_MAX_TOKENS`
- `ANTHROPIC_MESSAGES_URL`
