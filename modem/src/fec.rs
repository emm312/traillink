//! Hamming(7,4) encoder/decoder.
//! Maps 4 data bits into a 7-bit codeword.
//! Can detect and correct single-bit errors.

/// Encodes a 4-bit nibble (0-15) into a 7-bit codeword.
pub fn encode_nibble(data: u8) -> u8 {
    let d1 = data & 1;
    let d2 = (data >> 1) & 1;
    let d3 = (data >> 2) & 1;
    let d4 = (data >> 3) & 1;

    let p1 = d1 ^ d2 ^ d4;
    let p2 = d1 ^ d3 ^ d4;
    let p3 = d2 ^ d3 ^ d4;

    p1 | (p2 << 1) | (d1 << 2) | (p3 << 3) | (d2 << 4) | (d3 << 5) | (d4 << 6)
}

/// Decodes a 7-bit codeword (0-127), correcting up to 1 error.
/// Returns the corrected 4-bit nibble.
pub fn decode_nibble(codeword: u8) -> u8 {
    decode_nibble_with_correction(codeword).0
}

/// Decodes a 7-bit codeword and reports whether Hamming correction was applied.
pub fn decode_nibble_with_correction(codeword: u8) -> (u8, bool) {
    let p1 = codeword & 1;
    let p2 = (codeword >> 1) & 1;
    let d1 = (codeword >> 2) & 1;
    let p3 = (codeword >> 3) & 1;
    let d2 = (codeword >> 4) & 1;
    let d3 = (codeword >> 5) & 1;
    let d4 = (codeword >> 6) & 1;

    let s1 = p1 ^ d1 ^ d2 ^ d4;
    let s2 = p2 ^ d1 ^ d3 ^ d4;
    let s3 = p3 ^ d2 ^ d3 ^ d4;

    let syndrome = s1 | (s2 << 1) | (s3 << 2);

    let mut corrected = codeword & 0x7F;
    if syndrome > 0 {
        // Bit position to flip is syndrome - 1 (0-based index)
        corrected ^= 1 << (syndrome - 1);
    }

    // Extract corrected data bits
    let cd1 = (corrected >> 2) & 1;
    let cd2 = (corrected >> 4) & 1;
    let cd3 = (corrected >> 5) & 1;
    let cd4 = (corrected >> 6) & 1;

    (cd1 | (cd2 << 1) | (cd3 << 2) | (cd4 << 3), syndrome > 0)
}

/// Encode a slice of bytes using Hamming(7,4).
/// Each byte is split into two nibbles, producing two bytes of output (each containing 7 active bits).
pub fn encode_bytes(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len() * 2);
    for &byte in input {
        let low = byte & 0x0F;
        let high = (byte >> 4) & 0x0F;
        output.push(encode_nibble(low));
        output.push(encode_nibble(high));
    }
    output
}

/// Decode a slice of bytes using Hamming(7,4).
/// Every pair of bytes (7 active bits each) is decoded back into one output byte.
pub fn decode_bytes(input: &[u8]) -> Result<Vec<u8>, &'static str> {
    Ok(decode_bytes_with_stats(input)?.bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FecDecodeStats {
    pub bytes: Vec<u8>,
    pub corrections: usize,
}

/// Decode bytes using Hamming(7,4), reporting the number of corrected codewords.
pub fn decode_bytes_with_stats(input: &[u8]) -> Result<FecDecodeStats, &'static str> {
    if !input.len().is_multiple_of(2) {
        return Err("FEC input must have even length");
    }
    let mut output = Vec::with_capacity(input.len() / 2);
    let mut corrections = 0;
    for i in (0..input.len()).step_by(2) {
        let (low, low_corrected) = decode_nibble_with_correction(input[i]);
        let (high, high_corrected) = decode_nibble_with_correction(input[i + 1]);
        corrections += usize::from(low_corrected) + usize::from(high_corrected);
        output.push(low | (high << 4));
    }
    Ok(FecDecodeStats {
        bytes: output,
        corrections,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hamming_no_errors() {
        for val in 0..16 {
            let encoded = encode_nibble(val);
            let decoded = decode_nibble(encoded);
            assert_eq!(val, decoded);
        }
    }

    #[test]
    fn test_hamming_single_error_correction() {
        for val in 0..16 {
            let encoded = encode_nibble(val);
            // Test corrupting each of the 7 bits
            for bit in 0..7 {
                let corrupted = encoded ^ (1 << bit);
                let decoded = decode_nibble(corrupted);
                assert_eq!(
                    val, decoded,
                    "Failed correcting bit {} for value {}",
                    bit, val
                );
            }
        }
    }

    #[test]
    fn test_encode_decode_bytes() {
        let data = vec![0x12, 0xAB, 0xFF, 0x00];
        let encoded = encode_bytes(&data);
        assert_eq!(encoded.len(), 8);

        let decoded = decode_bytes(&encoded).unwrap();
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_encode_decode_bytes_with_errors() {
        let data = vec![0x42, 0x9F, 0x11];
        let mut encoded = encode_bytes(&data);

        // Corrupt exactly one bit in each byte
        for byte in encoded.iter_mut() {
            *byte ^= 1 << 4; // corrupt bit 4
        }

        let decoded = decode_bytes(&encoded).unwrap();
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_decode_bytes_reports_corrections() {
        let data = vec![0x42, 0x9F, 0x11];
        let mut encoded = encode_bytes(&data);
        encoded[0] ^= 1 << 4;
        encoded[3] ^= 1 << 2;
        encoded[5] ^= 1 << 1;

        let stats = decode_bytes_with_stats(&encoded).unwrap();
        assert_eq!(data, stats.bytes);
        assert_eq!(stats.corrections, 3);
    }
}
