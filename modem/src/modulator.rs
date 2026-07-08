use crate::frame::Frame;

pub struct Modulator {
    phase: f64,
}

impl Default for Modulator {
    fn default() -> Self {
        Self::new()
    }
}

impl Modulator {
    pub fn new() -> Self {
        Self { phase: 0.0 }
    }

    /// Reset phase accumulator.
    pub fn reset(&mut self) {
        self.phase = 0.0;
    }

    /// Modulate a Frame (with optional FEC) into a vector of 48 kHz f32 audio samples.
    pub fn modulate(&mut self, frame: &Frame, use_fec: bool, preamble_ms: usize) -> Vec<f32> {
        // 1. Serialize frame to bytes
        let mut data_bytes = frame.to_bytes();

        // 2. Apply FEC if requested
        if use_fec {
            data_bytes = crate::fec::encode_bytes(&data_bytes);
        }

        // 3. Assemble symbols
        let mut symbols = Vec::new();

        // Preamble: configurable duration of alternating 0 and 1 symbols
        let preamble_symbols = (preamble_ms * crate::SYMBOL_RATE) / 1000;
        let preamble_symbols = std::cmp::max(preamble_symbols, 2); // ensure at least alternating sequence
        for i in 0..preamble_symbols {
            symbols.push((i % 2) as u8);
        }

        // Sync word: 4 bytes (Big-endian serialization of SYNC_WORD)
        let sync_bytes = [
            (crate::SYNC_WORD >> 24) as u8,
            (crate::SYNC_WORD >> 16) as u8,
            (crate::SYNC_WORD >> 8) as u8,
            crate::SYNC_WORD as u8,
        ];
        for &b in &sync_bytes {
            symbols.extend(Self::byte_to_symbols(b));
        }

        // Payload/frame data bytes
        for &b in &data_bytes {
            symbols.extend(Self::byte_to_symbols(b));
        }

        // 4. Modulate symbols to samples
        let mut samples = Vec::with_capacity(symbols.len() * crate::SAMPLES_PER_SYMBOL);
        for &sym in &symbols {
            let tone_hz = Self::symbol_to_tone(sym);
            for _ in 0..crate::SAMPLES_PER_SYMBOL {
                // Generate sine wave sample, continuing the phase
                let val = self.phase.sin() as f32;
                samples.push(val);

                // Update phase accumulator
                // phase += 2 * pi * f / fs
                self.phase +=
                    2.0 * std::f64::consts::PI * (tone_hz as f64) / (crate::SAMPLE_RATE as f64);
                if self.phase >= 2.0 * std::f64::consts::PI {
                    self.phase -= 2.0 * std::f64::consts::PI;
                }
            }
        }

        samples
    }

    /// Convert a single byte to 4 symbols (2 bits each), LSB-first.
    pub fn byte_to_symbols(byte: u8) -> [u8; 4] {
        [
            byte & 0x03,
            (byte >> 2) & 0x03,
            (byte >> 4) & 0x03,
            (byte >> 6) & 0x03,
        ]
    }

    /// Map a symbol (0..3) to its tone frequency using Gray coding.
    pub fn symbol_to_tone(sym: u8) -> usize {
        let gray = (sym ^ (sym >> 1)) & 0x03;
        crate::TONES[gray as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::MsgType;

    #[test]
    fn test_modulate_length() {
        let frame = Frame::new(0, MsgType::Query, false, vec![42]).unwrap();
        let mut modulator = Modulator::new();
        let samples = modulator.modulate(&frame, false, 100);

        // Preamble: 100 ms at 600 symbols/s -> 60 symbols
        // Sync word: 4 bytes -> 16 symbols
        // Frame bytes: 3 (header) + 1 (payload) + 2 (crc of block 1) = 6 bytes -> 24 symbols
        // Total symbols = 60 + 16 + 24 = 100 symbols
        // Samples = 100 * 80 = 8000 samples
        assert_eq!(samples.len(), 100 * crate::SAMPLES_PER_SYMBOL);
    }
}
