use crc::{Algorithm, Crc};

pub const CRC_ALGO: Algorithm<u16> = Algorithm {
    width: 16,
    poly: 0x1021,
    init: 0xFFFF,
    refin: false,
    refout: false,
    xorout: 0x0000,
    check: 0x29B1,
    residue: 0x0000,
};

pub const CRC_CALC: Crc<u16> = Crc::<u16>::new(&CRC_ALGO);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Query = 0,
    Response = 1,
    Ack = 2,
    Broadcast = 3,
    SOS = 4,
    ImageChunk = 5,
}

impl MsgType {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(MsgType::Query),
            1 => Some(MsgType::Response),
            2 => Some(MsgType::Ack),
            3 => Some(MsgType::Broadcast),
            4 => Some(MsgType::SOS),
            5 => Some(MsgType::ImageChunk),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub version: u8,        // 3 bits
    pub msg_type: MsgType,  // 4 bits
    pub has_location: bool, // 1 bit (flag bit in ver+type)
    pub payload: Vec<u8>,   // up to MAX_FRAME_PAYLOAD_BYTES bytes
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrcBlockReport {
    pub block_idx: usize,
    pub received: u16,
    pub expected: u16,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameParseReport {
    pub frame: Frame,
    pub crc_blocks: Vec<CrcBlockReport>,
}

impl Frame {
    pub fn new(
        version: u8,
        msg_type: MsgType,
        has_location: bool,
        payload: Vec<u8>,
    ) -> Result<Self, &'static str> {
        if version > 7 {
            return Err("Version must be 3 bits (0-7)");
        }
        if payload.len() > crate::MAX_FRAME_PAYLOAD_BYTES {
            return Err("Payload cannot exceed 2048 bytes");
        }
        Ok(Self {
            version,
            msg_type,
            has_location,
            payload,
        })
    }

    /// Encode the frame to raw bytes with interleaved CRCs every 255 bytes:
    /// [ver+type, len_hi, len_lo, block0..., crc0_hi, crc0_lo, block1..., crc1_hi, crc1_lo, ...]
    pub fn to_bytes(&self) -> Vec<u8> {
        let payload_len = self.payload.len();
        let expected_blocks = if payload_len == 0 {
            1
        } else {
            payload_len.div_ceil(crate::CRC_BLOCK_PAYLOAD_BYTES)
        };
        let mut bytes = Vec::with_capacity(
            crate::FRAME_HEADER_BYTES + payload_len + crate::CRC_BYTES * expected_blocks,
        );

        // ver+type (8 bits):
        // Bit 7: has_location
        // Bits 6-4: version
        // Bits 3-0: msg_type
        let mut ver_type = self.msg_type as u8 & 0x0F;
        ver_type |= (self.version & 0x07) << 4;
        if self.has_location {
            ver_type |= 0x80;
        }

        bytes.push(ver_type);
        bytes.push((payload_len >> 8) as u8);
        bytes.push((payload_len & 0xFF) as u8);

        if payload_len == 0 {
            // Edge case: empty payload, calculate CRC over just the header
            let crc_val = CRC_CALC.checksum(&bytes[0..3]);
            bytes.push((crc_val >> 8) as u8);
            bytes.push((crc_val & 0xFF) as u8);
        } else {
            for block_idx in 0..expected_blocks {
                let start = block_idx * crate::CRC_BLOCK_PAYLOAD_BYTES;
                let end = std::cmp::min(
                    (block_idx + 1) * crate::CRC_BLOCK_PAYLOAD_BYTES,
                    payload_len,
                );
                let chunk = &self.payload[start..end];

                bytes.extend_from_slice(chunk);

                let crc_val = if block_idx == 0 {
                    // First block CRC covers header + first block data
                    let mut data_to_crc = Vec::with_capacity(3 + chunk.len());
                    data_to_crc.extend_from_slice(&bytes[0..3]);
                    data_to_crc.extend_from_slice(chunk);
                    CRC_CALC.checksum(&data_to_crc)
                } else {
                    // Subsequent block CRCs cover only that block
                    CRC_CALC.checksum(chunk)
                };

                bytes.push((crc_val >> 8) as u8);
                bytes.push((crc_val & 0xFF) as u8);
            }
        }

        bytes
    }

    /// Parse a frame from raw bytes, validating length and all interleaved CRCs.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, &'static str> {
        Ok(Self::from_bytes_with_crc_report(bytes)?.frame)
    }

    /// Parse a frame and return per-block CRC metadata.
    pub fn from_bytes_with_crc_report(bytes: &[u8]) -> Result<FrameParseReport, &'static str> {
        if bytes.len() < 5 {
            return Err("Frame too short");
        }

        let ver_type = bytes[0];
        let len = ((bytes[1] as usize) << 8) | (bytes[2] as usize);

        if len > crate::MAX_FRAME_PAYLOAD_BYTES {
            return Err("Parsed frame payload size too large");
        }

        let expected_blocks = if len == 0 {
            1
        } else {
            len.div_ceil(crate::CRC_BLOCK_PAYLOAD_BYTES)
        };

        let mut payload = Vec::with_capacity(len);
        let mut current_byte_idx = 3;
        let mut crc_blocks = Vec::with_capacity(expected_blocks);

        for block_idx in 0..expected_blocks {
            let block_size = if len == 0 {
                0
            } else if block_idx == expected_blocks - 1 {
                len - block_idx * crate::CRC_BLOCK_PAYLOAD_BYTES
            } else {
                crate::CRC_BLOCK_PAYLOAD_BYTES
            };

            if current_byte_idx + block_size + 2 > bytes.len() {
                return Err("Frame size mismatch or truncated data");
            }

            let chunk = &bytes[current_byte_idx..current_byte_idx + block_size];
            let rx_crc = ((bytes[current_byte_idx + block_size] as u16) << 8)
                | (bytes[current_byte_idx + block_size + 1] as u16);

            let expected_crc = if block_idx == 0 {
                let mut data_to_crc = Vec::with_capacity(3 + chunk.len());
                data_to_crc.extend_from_slice(&bytes[0..3]);
                data_to_crc.extend_from_slice(chunk);
                CRC_CALC.checksum(&data_to_crc)
            } else {
                CRC_CALC.checksum(chunk)
            };

            crc_blocks.push(CrcBlockReport {
                block_idx,
                received: rx_crc,
                expected: expected_crc,
                passed: rx_crc == expected_crc,
            });

            if rx_crc != expected_crc {
                return Err("CRC check failed");
            }

            payload.extend_from_slice(chunk);
            current_byte_idx += block_size + 2;
        }

        if current_byte_idx != bytes.len() {
            return Err("Trailing extra bytes in frame");
        }

        let has_location = (ver_type & 0x80) != 0;
        let version = (ver_type >> 4) & 0x07;
        let msg_type_val = ver_type & 0x0F;
        let msg_type = MsgType::from_u8(msg_type_val).ok_or("Invalid message type")?;

        Ok(FrameParseReport {
            frame: Self {
                version,
                msg_type,
                has_location,
                payload,
            },
            crc_blocks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_serialization() {
        let frame = Frame::new(1, MsgType::Query, true, vec![1, 2, 3, 4]).unwrap();
        let bytes = frame.to_bytes();
        let parsed = Frame::from_bytes(&bytes).unwrap();
        assert_eq!(frame, parsed);
    }

    #[test]
    fn test_crc_report() {
        let frame = Frame::new(1, MsgType::Query, true, vec![1, 2, 3, 4]).unwrap();
        let bytes = frame.to_bytes();
        let report = Frame::from_bytes_with_crc_report(&bytes).unwrap();
        assert_eq!(frame, report.frame);
        assert_eq!(report.crc_blocks.len(), 1);
        assert!(report.crc_blocks[0].passed);
    }

    #[test]
    fn test_corrupt_crc() {
        let frame = Frame::new(1, MsgType::Query, true, vec![1, 2, 3, 4]).unwrap();
        let mut bytes = frame.to_bytes();
        // Corrupt one byte of payload (index 3 is first payload byte since header is 3 bytes)
        bytes[3] ^= 0xFF;
        assert!(Frame::from_bytes(&bytes).is_err());
    }

    #[test]
    fn test_large_frame_serialization() {
        let mut large_payload = vec![0u8; 1234];
        for (i, byte) in large_payload.iter_mut().enumerate().take(1234) {
            *byte = (i & 0xFF) as u8;
        }
        let frame = Frame::new(2, MsgType::ImageChunk, false, large_payload).unwrap();
        let bytes = frame.to_bytes();

        // Header (3) + 1234 bytes + 5 blocks (each block has a 2-byte CRC. 1234/255 = 4.8 blocks -> 5 blocks)
        // 5 blocks * 2 bytes = 10 bytes CRC. Total = 3 + 1234 + 10 = 1247 bytes.
        assert_eq!(bytes.len(), 1247);

        let parsed = Frame::from_bytes(&bytes).unwrap();
        assert_eq!(frame, parsed);
    }

    #[test]
    fn test_empty_frame_serialization() {
        let frame = Frame::new(1, MsgType::Ack, false, vec![]).unwrap();
        let bytes = frame.to_bytes();
        assert_eq!(bytes.len(), 5); // 3 header + 2 CRC
        let parsed = Frame::from_bytes(&bytes).unwrap();
        assert_eq!(frame, parsed);
    }
}
