#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageChunk {
    pub image_id: u32,
    pub chunk_idx: u16,
    pub total_chunks: u16,
    pub data: Vec<u8>,
}

pub fn image_chunk_count(data_len: usize) -> Result<u16, &'static str> {
    if data_len == 0 {
        return Err("Image data cannot be empty");
    }

    let chunks = data_len.div_ceil(crate::IMAGE_CHUNK_DATA_BYTES);
    u16::try_from(chunks).map_err(|_| "Image requires too many chunks")
}

fn max_image_chunks() -> usize {
    crate::MAX_IMAGE_BYTES.div_ceil(crate::IMAGE_CHUNK_DATA_BYTES)
}

pub fn encode_image_chunk_payload(chunk: &ImageChunk) -> Result<Vec<u8>, &'static str> {
    if chunk.total_chunks == 0 {
        return Err("Image chunk total must be non-zero");
    }
    if chunk.chunk_idx >= chunk.total_chunks {
        return Err("Image chunk index out of range");
    }
    if chunk.data.len() > crate::MAX_FRAME_PAYLOAD_BYTES - crate::IMAGE_CHUNK_HEADER_BYTES {
        return Err("Image chunk payload too large");
    }
    if chunk.data.is_empty() {
        return Err("Image chunk data cannot be empty");
    }
    if usize::from(chunk.total_chunks) > max_image_chunks() {
        return Err("Image chunk total exceeds maximum image size");
    }
    if chunk.data.len() > crate::IMAGE_CHUNK_DATA_BYTES {
        return Err("Image chunk data exceeds configured chunk size");
    }

    let mut payload = Vec::with_capacity(crate::IMAGE_CHUNK_HEADER_BYTES + chunk.data.len());
    payload.extend_from_slice(&chunk.image_id.to_be_bytes());
    payload.extend_from_slice(&chunk.chunk_idx.to_be_bytes());
    payload.extend_from_slice(&chunk.total_chunks.to_be_bytes());
    payload.extend_from_slice(&chunk.data);
    Ok(payload)
}

pub fn parse_image_chunk_payload(payload: &[u8]) -> Result<ImageChunk, &'static str> {
    if payload.len() < crate::IMAGE_CHUNK_HEADER_BYTES {
        return Err("Image chunk payload too short");
    }

    let image_id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let chunk_idx = u16::from_be_bytes([payload[4], payload[5]]);
    let total_chunks = u16::from_be_bytes([payload[6], payload[7]]);

    if total_chunks == 0 {
        return Err("Image chunk total must be non-zero");
    }
    if chunk_idx >= total_chunks {
        return Err("Image chunk index out of range");
    }
    if usize::from(total_chunks) > max_image_chunks() {
        return Err("Image chunk total exceeds maximum image size");
    }

    let data = payload[crate::IMAGE_CHUNK_HEADER_BYTES..].to_vec();
    if data.is_empty() {
        return Err("Image chunk data cannot be empty");
    }
    if data.len() > crate::IMAGE_CHUNK_DATA_BYTES {
        return Err("Image chunk data exceeds configured chunk size");
    }

    Ok(ImageChunk {
        image_id,
        chunk_idx,
        total_chunks,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_chunk_payload_round_trip() {
        let chunk = ImageChunk {
            image_id: 0xAABBCCDD,
            chunk_idx: 2,
            total_chunks: 4,
            data: vec![1, 2, 3, 4],
        };

        let payload = encode_image_chunk_payload(&chunk).unwrap();
        assert_eq!(parse_image_chunk_payload(&payload).unwrap(), chunk);
    }

    #[test]
    fn test_image_chunk_rejects_bad_metadata() {
        let zero_total = ImageChunk {
            image_id: 1,
            chunk_idx: 0,
            total_chunks: 0,
            data: vec![1],
        };
        assert!(encode_image_chunk_payload(&zero_total).is_err());

        let out_of_range = ImageChunk {
            image_id: 1,
            chunk_idx: 2,
            total_chunks: 2,
            data: vec![1],
        };
        assert!(encode_image_chunk_payload(&out_of_range).is_err());
    }

    #[test]
    fn test_image_chunk_count_rejects_overflow() {
        let too_large = (u16::MAX as usize + 1) * crate::IMAGE_CHUNK_DATA_BYTES;
        assert!(image_chunk_count(too_large).is_err());
    }

    #[test]
    fn test_image_chunk_rejects_impossible_total() {
        let payload = [
            0, 0, 0, 1, // image id
            0, 0, // chunk index
            0xFF, 0xFF, // impossible total for configured max image size
            1, 2, 3,
        ];

        assert!(parse_image_chunk_payload(&payload).is_err());
    }
}
