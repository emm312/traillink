use crate::frame::Frame;

pub struct Demodulator;

#[derive(Debug, Clone)]
pub struct DecodedFrameReport {
    pub frame: Option<Frame>,
    pub use_fec: bool,
    pub sync_score: f32,
    pub snr_db: Option<f32>,
    pub fec_corrections: usize,
    pub crc_pass: bool,
    pub error: Option<&'static str>,
}

#[derive(Clone, Copy)]
struct SymbolQuality {
    sym: u8,
    selected_power: f32,
    other_power: f32,
}

impl Default for Demodulator {
    fn default() -> Self {
        Self::new()
    }
}

impl Demodulator {
    pub fn new() -> Self {
        Self
    }

    /// Scan the samples to find the sync word, then decode the Frame.
    pub fn demodulate(&self, samples: &[f32], use_fec: bool) -> Result<Frame, &'static str> {
        // Pad input samples with one symbol worth of zeros to tolerate late sync alignments
        let mut padded_storage;
        let samples = if samples.is_empty() {
            samples
        } else {
            padded_storage = Vec::with_capacity(samples.len() + crate::SAMPLES_PER_SYMBOL);
            padded_storage.extend_from_slice(samples);
            padded_storage.resize(samples.len() + crate::SAMPLES_PER_SYMBOL, 0.0);
            &padded_storage[..]
        };

        // 1. Convert SYNC_WORD to expected symbols
        let sync_bytes = [
            (crate::SYNC_WORD >> 24) as u8,
            (crate::SYNC_WORD >> 16) as u8,
            (crate::SYNC_WORD >> 8) as u8,
            crate::SYNC_WORD as u8,
        ];
        let mut expected_sync_symbols = Vec::with_capacity(16);
        for &b in &sync_bytes {
            expected_sync_symbols
                .extend_from_slice(&crate::modulator::Modulator::byte_to_symbols(b));
        }

        // 2. Coarse search for sync word
        let mut coarse_best_t = 0;
        let mut coarse_best_score = -1.0;

        let step_size = 8;
        let search_limit = if samples.len() > 16 * crate::SAMPLES_PER_SYMBOL {
            samples.len() - 16 * crate::SAMPLES_PER_SYMBOL
        } else {
            0
        };

        for t in (0..search_limit).step_by(step_size) {
            let score = self.compute_sync_score(samples, t, &expected_sync_symbols);
            if score > coarse_best_score {
                coarse_best_score = score;
                coarse_best_t = t;
            }
        }

        // We require a coarse score of at least 9.5 (out of 16.0) to proceed
        if coarse_best_score < 9.5 {
            return Err("Sync word not found (coarse search failed)");
        }

        // 3. Fine search around the coarse peak
        let mut fine_best_t = coarse_best_t;
        let mut fine_best_score = coarse_best_score;

        let range_start = coarse_best_t.saturating_sub(8);
        let range_end = std::cmp::min(coarse_best_t + 8, search_limit);

        for t in range_start..=range_end {
            let score = self.compute_sync_score(samples, t, &expected_sync_symbols);
            if score > fine_best_score {
                fine_best_score = score;
                fine_best_t = t;
            }
        }

        // Final threshold check (e.g. 10.5 out of 16.0)
        if fine_best_score < 10.5 {
            return Err("Sync word not found (fine search failed)");
        }

        // 4. Decode payload
        let payload_start_t = fine_best_t + 16 * crate::SAMPLES_PER_SYMBOL;

        // Let's determine how many bytes we need to decode.
        // We first decode the header (ver_type and len).
        // Without FEC: 3 bytes (12 symbols).
        // With FEC: 6 bytes (24 symbols).
        let header_bytes_count = if use_fec { 6 } else { 3 };
        let header_symbols_count = header_bytes_count * 4;

        let mut header_symbols = Vec::with_capacity(header_symbols_count);
        let mut current_t = payload_start_t;

        for _ in 0..header_symbols_count {
            if current_t + crate::SAMPLES_PER_SYMBOL > samples.len() {
                return Err("Samples truncated during header decoding");
            }

            // Early-late timing tracker
            if current_t >= 2 && current_t + crate::SAMPLES_PER_SYMBOL + 2 <= samples.len() {
                let (_, p_center) = self.demodulate_symbol_and_power(
                    &samples[current_t..current_t + crate::SAMPLES_PER_SYMBOL],
                );
                let (_, p_early) = self.demodulate_symbol_and_power(
                    &samples[current_t - 2..current_t + crate::SAMPLES_PER_SYMBOL - 2],
                );
                let (_, p_late) = self.demodulate_symbol_and_power(
                    &samples[current_t + 2..current_t + crate::SAMPLES_PER_SYMBOL + 2],
                );

                if p_early > p_center && p_early > p_late {
                    current_t -= 2;
                } else if p_late > p_center && p_late > p_early {
                    current_t += 2;
                }
            }

            let sym_samples = &samples[current_t..current_t + crate::SAMPLES_PER_SYMBOL];
            let (sym, _) = self.demodulate_symbol_and_power(sym_samples);
            header_symbols.push(sym);
            current_t += crate::SAMPLES_PER_SYMBOL;
        }

        let mut header_bytes = self.symbols_to_bytes(&header_symbols);
        if use_fec {
            header_bytes = crate::fec::decode_bytes(&header_bytes)?;
        }

        let ver_type = header_bytes[0];
        let len = ((header_bytes[1] as usize) << 8) | (header_bytes[2] as usize);

        // Total body size in bytes with interleaved CRCs every 255 bytes
        let expected_blocks = if len == 0 {
            1
        } else {
            len.div_ceil(crate::CRC_BLOCK_PAYLOAD_BYTES)
        };
        let body_bytes_count = len + 2 * expected_blocks;
        let encoded_body_bytes_count = if use_fec {
            body_bytes_count * 2
        } else {
            body_bytes_count
        };
        let body_symbols_count = encoded_body_bytes_count * 4;

        let mut body_symbols = Vec::with_capacity(body_symbols_count);
        for _ in 0..body_symbols_count {
            if current_t + crate::SAMPLES_PER_SYMBOL > samples.len() {
                return Err("Samples truncated during body decoding");
            }

            // Early-late timing tracker
            if current_t >= 2 && current_t + crate::SAMPLES_PER_SYMBOL + 2 <= samples.len() {
                let (_, p_center) = self.demodulate_symbol_and_power(
                    &samples[current_t..current_t + crate::SAMPLES_PER_SYMBOL],
                );
                let (_, p_early) = self.demodulate_symbol_and_power(
                    &samples[current_t - 2..current_t + crate::SAMPLES_PER_SYMBOL - 2],
                );
                let (_, p_late) = self.demodulate_symbol_and_power(
                    &samples[current_t + 2..current_t + crate::SAMPLES_PER_SYMBOL + 2],
                );

                if p_early > p_center && p_early > p_late {
                    current_t -= 2;
                } else if p_late > p_center && p_late > p_early {
                    current_t += 2;
                }
            }

            let sym_samples = &samples[current_t..current_t + crate::SAMPLES_PER_SYMBOL];
            let (sym, _) = self.demodulate_symbol_and_power(sym_samples);
            body_symbols.push(sym);
            current_t += crate::SAMPLES_PER_SYMBOL;
        }

        let mut body_bytes = self.symbols_to_bytes(&body_symbols);
        if use_fec {
            body_bytes = crate::fec::decode_bytes(&body_bytes)?;
        }

        // Reconstruct the full raw frame bytes: [ver_type, len_hi, len_lo, body_bytes...]
        let mut full_frame_bytes = Vec::with_capacity(3 + body_bytes.len());
        full_frame_bytes.push(ver_type);
        full_frame_bytes.push((len >> 8) as u8);
        full_frame_bytes.push((len & 0xFF) as u8);
        full_frame_bytes.extend_from_slice(&body_bytes);

        // Parse Frame
        Frame::from_bytes(&full_frame_bytes)
    }

    /// Scan the samples to find and decode all Frames present in the audio stream.
    pub fn demodulate_multi(&self, samples: &[f32], use_fec: bool) -> Vec<Frame> {
        self.demodulate_multi_with_diagnostics(samples, use_fec)
            .into_iter()
            .filter_map(|report| report.frame)
            .collect()
    }

    /// Scan samples and return decoded frames with link-quality diagnostics.
    pub fn demodulate_multi_with_diagnostics(
        &self,
        samples: &[f32],
        use_fec: bool,
    ) -> Vec<DecodedFrameReport> {
        // Pad input samples with one symbol worth of zeros to tolerate late sync alignments
        let mut padded_storage;
        let samples_ref = if samples.is_empty() {
            samples
        } else {
            padded_storage = Vec::with_capacity(samples.len() + crate::SAMPLES_PER_SYMBOL);
            padded_storage.extend_from_slice(samples);
            padded_storage.resize(samples.len() + crate::SAMPLES_PER_SYMBOL, 0.0);
            &padded_storage[..]
        };

        let mut reports = Vec::new();
        let mut peak_coarse_score = 0.0f32;

        // 1. Convert SYNC_WORD to expected symbols
        let sync_bytes = [
            (crate::SYNC_WORD >> 24) as u8,
            (crate::SYNC_WORD >> 16) as u8,
            (crate::SYNC_WORD >> 8) as u8,
            crate::SYNC_WORD as u8,
        ];
        let mut expected_sync_symbols = Vec::with_capacity(16);
        for &b in &sync_bytes {
            expected_sync_symbols
                .extend_from_slice(&crate::modulator::Modulator::byte_to_symbols(b));
        }

        let step_size = 8;
        let mut t = 0;

        while t < samples_ref.len() {
            let search_limit = if samples_ref.len() > t + 16 * crate::SAMPLES_PER_SYMBOL {
                samples_ref.len() - 16 * crate::SAMPLES_PER_SYMBOL
            } else {
                break;
            };

            let score = self.compute_sync_score(samples_ref, t, &expected_sync_symbols);
            if score > peak_coarse_score {
                peak_coarse_score = score;
            }
            if score >= 9.5 {
                // We found a local coarse sync candidate!
                // Let's do a fine search around this peak in the neighborhood [t-8, t+8]
                let mut fine_best_t = t;
                let mut fine_best_score = score;

                let range_start = t.saturating_sub(8);
                let range_end = std::cmp::min(t + 8, search_limit);

                for ft in range_start..=range_end {
                    let fscore = self.compute_sync_score(samples_ref, ft, &expected_sync_symbols);
                    if fscore > fine_best_score {
                        fine_best_score = fscore;
                        fine_best_t = ft;
                    }
                }

                if fine_best_score < 10.5 {
                    // Not a real sync, skip past this candidate to avoid infinite loop
                    t += step_size;
                    continue;
                }

                // Decode this frame
                let payload_start_t = fine_best_t + 16 * crate::SAMPLES_PER_SYMBOL;

                // Try decoding header
                let header_bytes_count = if use_fec { 6 } else { 3 };
                let header_symbols_count = header_bytes_count * 4;
                let header_samples_count = header_symbols_count * crate::SAMPLES_PER_SYMBOL;

                if payload_start_t + header_samples_count > samples_ref.len() {
                    break; // Truncated
                }

                let mut header_symbols = Vec::with_capacity(header_symbols_count);
                let mut current_t = payload_start_t;
                let mut selected_power_sum = 0.0f32;
                let mut other_power_sum = 0.0f32;
                let mut symbol_count = 0usize;

                let mut header_decode_failed = false;
                for _ in 0..header_symbols_count {
                    // Early-late timing tracker
                    if current_t >= 2
                        && current_t + crate::SAMPLES_PER_SYMBOL + 2 <= samples_ref.len()
                    {
                        let (_, p_center) = self.demodulate_symbol_and_power(
                            &samples_ref[current_t..current_t + crate::SAMPLES_PER_SYMBOL],
                        );
                        let (_, p_early) = self.demodulate_symbol_and_power(
                            &samples_ref[current_t - 2..current_t + crate::SAMPLES_PER_SYMBOL - 2],
                        );
                        let (_, p_late) = self.demodulate_symbol_and_power(
                            &samples_ref[current_t + 2..current_t + crate::SAMPLES_PER_SYMBOL + 2],
                        );

                        if p_early > p_center && p_early > p_late {
                            current_t -= 2;
                        } else if p_late > p_center && p_late > p_early {
                            current_t += 2;
                        }
                    }

                    let sym_samples =
                        &samples_ref[current_t..current_t + crate::SAMPLES_PER_SYMBOL];
                    let quality = self.demodulate_symbol_quality(sym_samples);
                    selected_power_sum += quality.selected_power;
                    other_power_sum += quality.other_power;
                    symbol_count += 1;
                    header_symbols.push(quality.sym);
                    current_t += crate::SAMPLES_PER_SYMBOL;
                }

                let mut header_bytes = self.symbols_to_bytes(&header_symbols);
                let mut fec_corrections = 0;
                if use_fec {
                    if let Ok(decoded_header) = crate::fec::decode_bytes_with_stats(&header_bytes) {
                        fec_corrections += decoded_header.corrections;
                        header_bytes = decoded_header.bytes;
                    } else {
                        header_decode_failed = true;
                    }
                }

                if header_decode_failed {
                    reports.push(DecodedFrameReport {
                        frame: None,
                        use_fec,
                        sync_score: fine_best_score,
                        snr_db: estimate_snr_db(selected_power_sum, other_power_sum, symbol_count),
                        fec_corrections,
                        crc_pass: false,
                        error: Some("FEC header decode failed"),
                    });
                    // Header decode fail, skip past the sync word to continue scanning
                    t = fine_best_t + 16 * crate::SAMPLES_PER_SYMBOL;
                    continue;
                }

                let ver_type = header_bytes[0];
                let len = ((header_bytes[1] as usize) << 8) | (header_bytes[2] as usize);

                let expected_blocks = if len == 0 {
                    1
                } else {
                    len.div_ceil(crate::CRC_BLOCK_PAYLOAD_BYTES)
                };
                let body_bytes_count = len + 2 * expected_blocks;
                let encoded_body_bytes_count = if use_fec {
                    body_bytes_count * 2
                } else {
                    body_bytes_count
                };
                let body_symbols_count = encoded_body_bytes_count * 4;
                let body_samples_count = body_symbols_count * crate::SAMPLES_PER_SYMBOL;

                if payload_start_t + header_samples_count + body_samples_count > samples_ref.len() {
                    // Frame body truncated, done
                    break;
                }

                let mut body_symbols = Vec::with_capacity(body_symbols_count);
                let mut body_decode_failed = false;
                for _ in 0..body_symbols_count {
                    // Early-late timing tracker
                    if current_t >= 2
                        && current_t + crate::SAMPLES_PER_SYMBOL + 2 <= samples_ref.len()
                    {
                        let (_, p_center) = self.demodulate_symbol_and_power(
                            &samples_ref[current_t..current_t + crate::SAMPLES_PER_SYMBOL],
                        );
                        let (_, p_early) = self.demodulate_symbol_and_power(
                            &samples_ref[current_t - 2..current_t + crate::SAMPLES_PER_SYMBOL - 2],
                        );
                        let (_, p_late) = self.demodulate_symbol_and_power(
                            &samples_ref[current_t + 2..current_t + crate::SAMPLES_PER_SYMBOL + 2],
                        );

                        if p_early > p_center && p_early > p_late {
                            current_t -= 2;
                        } else if p_late > p_center && p_late > p_early {
                            current_t += 2;
                        }
                    }

                    let sym_samples =
                        &samples_ref[current_t..current_t + crate::SAMPLES_PER_SYMBOL];
                    let quality = self.demodulate_symbol_quality(sym_samples);
                    selected_power_sum += quality.selected_power;
                    other_power_sum += quality.other_power;
                    symbol_count += 1;
                    body_symbols.push(quality.sym);
                    current_t += crate::SAMPLES_PER_SYMBOL;
                }

                let mut body_bytes = self.symbols_to_bytes(&body_symbols);
                if use_fec {
                    if let Ok(decoded_body) = crate::fec::decode_bytes_with_stats(&body_bytes) {
                        fec_corrections += decoded_body.corrections;
                        body_bytes = decoded_body.bytes;
                    } else {
                        body_decode_failed = true;
                    }
                }

                if body_decode_failed {
                    reports.push(DecodedFrameReport {
                        frame: None,
                        use_fec,
                        sync_score: fine_best_score,
                        snr_db: estimate_snr_db(selected_power_sum, other_power_sum, symbol_count),
                        fec_corrections,
                        crc_pass: false,
                        error: Some("FEC body decode failed"),
                    });
                    // Body decode fail, skip past the frame to continue scanning
                    t = current_t;
                    continue;
                }

                let mut full_frame_bytes = Vec::with_capacity(3 + body_bytes.len());
                full_frame_bytes.push(ver_type);
                full_frame_bytes.push((len >> 8) as u8);
                full_frame_bytes.push((len & 0xFF) as u8);
                full_frame_bytes.extend_from_slice(&body_bytes);

                match Frame::from_bytes_with_crc_report(&full_frame_bytes) {
                    Ok(report) => {
                        reports.push(DecodedFrameReport {
                            frame: Some(report.frame),
                            use_fec,
                            sync_score: fine_best_score,
                            snr_db: estimate_snr_db(
                                selected_power_sum,
                                other_power_sum,
                                symbol_count,
                            ),
                            fec_corrections,
                            crc_pass: true,
                            error: None,
                        });
                    }
                    Err(error) => {
                        reports.push(DecodedFrameReport {
                            frame: None,
                            use_fec,
                            sync_score: fine_best_score,
                            snr_db: estimate_snr_db(
                                selected_power_sum,
                                other_power_sum,
                                symbol_count,
                            ),
                            fec_corrections,
                            crc_pass: false,
                            error: Some(error),
                        });
                    }
                }

                // Move search index past this entire successfully decoded frame
                t = current_t;
                continue;
            }
            t += step_size;
        }

        // Peak coarse sync score debug print removed for clean logging

        reports
    }

    /// Compute Goertzel-based sync score at starting index `t`.
    fn compute_sync_score(&self, samples: &[f32], t: usize, expected_symbols: &[u8]) -> f32 {
        let mut total_score = 0.0;
        for (i, &expected_sym) in expected_symbols.iter().enumerate() {
            let offset = t + i * crate::SAMPLES_PER_SYMBOL;
            if offset + crate::SAMPLES_PER_SYMBOL > samples.len() {
                return -1.0;
            }
            let sym_samples = &samples[offset..offset + crate::SAMPLES_PER_SYMBOL];

            let mut powers = [0.0; 4];
            let mut sum_powers = 0.0;
            for (s, &tone_hz) in crate::TONES.iter().enumerate() {
                let p = self.goertzel_power(sym_samples, tone_hz as f64);
                powers[s] = p;
                sum_powers += p;
            }

            if sum_powers > 1e-6 {
                // Tone index is mapped via Gray coding
                let expected_tone_idx = ((expected_sym ^ (expected_sym >> 1)) & 0x03) as usize;
                let expected_power = powers[expected_tone_idx];
                total_score += expected_power / sum_powers;
            } else {
                total_score += 0.25;
            }
        }
        total_score
    }

    /// Demodulate a single symbol window and return the symbol and its power.
    fn demodulate_symbol_and_power(&self, sym_samples: &[f32]) -> (u8, f32) {
        let quality = self.demodulate_symbol_quality(sym_samples);
        (quality.sym, quality.selected_power)
    }

    fn demodulate_symbol_quality(&self, sym_samples: &[f32]) -> SymbolQuality {
        let mut max_power = -1.0;
        let mut max_tone_idx = 0;
        let mut total_power = 0.0;
        for (s, &tone_hz) in crate::TONES.iter().enumerate() {
            let p = self.goertzel_power(sym_samples, tone_hz as f64);
            total_power += p;
            if p > max_power {
                max_power = p;
                max_tone_idx = s;
            }
        }
        let sym = match max_tone_idx {
            0 => 0,
            1 => 1,
            2 => 3,
            3 => 2,
            _ => 0,
        };
        SymbolQuality {
            sym,
            selected_power: max_power.max(0.0),
            other_power: (total_power - max_power).max(0.0),
        }
    }

    /// Goertzel power calculation.
    fn goertzel_power(&self, samples: &[f32], freq: f64) -> f32 {
        let omega = 2.0 * std::f64::consts::PI * freq / (crate::SAMPLE_RATE as f64);
        let coeff = 2.0 * omega.cos();
        let mut s1 = 0.0;
        let mut s2 = 0.0;
        for &x in samples {
            let s0 = (x as f64) + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2) as f32
    }

    /// Group 4 symbols into one byte, LSB-first.
    fn symbols_to_bytes(&self, symbols: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(symbols.len() / 4);
        for chunk in symbols.chunks_exact(4) {
            let byte = chunk[0] | (chunk[1] << 2) | (chunk[2] << 4) | (chunk[3] << 6);
            bytes.push(byte);
        }
        bytes
    }
}

fn estimate_snr_db(
    signal_power_sum: f32,
    noise_power_sum: f32,
    sample_count: usize,
) -> Option<f32> {
    if sample_count == 0 {
        return None;
    }
    let signal = (signal_power_sum / sample_count as f32).max(1e-9);
    let noise = (noise_power_sum / sample_count as f32).max(1e-9);
    Some(10.0 * (signal / noise).log10())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, MsgType};
    use crate::modulator::Modulator;

    #[test]
    fn test_goertzel_single_tones() {
        let demod = Demodulator::new();
        for &tone_hz in &crate::TONES {
            // Generate SAMPLES_PER_SYMBOL samples of this tone
            let mut samples = Vec::new();
            let mut phase: f64 = 0.0;
            for _ in 0..crate::SAMPLES_PER_SYMBOL {
                samples.push(phase.sin() as f32);
                phase += 2.0 * std::f64::consts::PI * (tone_hz as f64) / 48000.0;
            }

            // Check Goertzel power at all 4 tones
            let mut powers = [0.0; 4];
            for (i, &t_hz) in crate::TONES.iter().enumerate() {
                powers[i] = demod.goertzel_power(&samples, t_hz as f64);
            }

            println!("Tone {} Hz: {:?}", tone_hz, powers);
            // Power of the target tone should be significantly higher than others
            let target_idx = crate::TONES.iter().position(|&x| x == tone_hz).unwrap();
            for i in 0..4 {
                if i != target_idx && powers[target_idx] <= powers[i] {
                    println!(
                        "WARNING: Tone {} power is {}, but non-target tone {} power is {}",
                        tone_hz,
                        powers[target_idx],
                        crate::TONES[i],
                        powers[i]
                    );
                }
            }
        }
    }

    #[test]
    fn test_sync_score_profile() {
        let frame = Frame::new(1, MsgType::Response, false, vec![1, 2, 3]).unwrap();
        let mut mod_obj = Modulator::new();
        let samples = mod_obj.modulate(&frame, false, 100);
        let demod = Demodulator::new();

        let sync_bytes = [
            (crate::SYNC_WORD >> 24) as u8,
            (crate::SYNC_WORD >> 16) as u8,
            (crate::SYNC_WORD >> 8) as u8,
            crate::SYNC_WORD as u8,
        ];
        let mut expected_sync_symbols = Vec::new();
        for &b in &sync_bytes {
            expected_sync_symbols.extend_from_slice(&Modulator::byte_to_symbols(b));
        }

        // Let's print the score around the actual sync start (60 symbols * 80 samples = 4800)
        for offset in (4800 - 80)..=(4800 + 80) {
            let score = demod.compute_sync_score(&samples, offset, &expected_sync_symbols);
            if score > 0.0 {
                println!("offset: {}, score: {}", offset, score);
            }
        }
    }

    #[test]
    fn diagnostics_report_successful_fec_decode() {
        let frame = Frame::new(1, MsgType::Response, false, b"status".to_vec()).unwrap();
        let mut modulator = Modulator::new();
        let samples = modulator.modulate(&frame, true, 100);
        let demod = Demodulator::new();

        let reports = demod.demodulate_multi_with_diagnostics(&samples, true);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].frame.as_ref(), Some(&frame));
        assert!(reports[0].use_fec);
        assert!(reports[0].crc_pass);
        assert_eq!(reports[0].fec_corrections, 0);
        assert!(reports[0].snr_db.is_some());
        assert!(reports[0].error.is_none());
    }
}
