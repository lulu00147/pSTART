//! Length-prefixed framing.
//!
//! Wire format: `u32` little-endian payload length followed by the postcard
//! payload. The length is validated before any payload allocation.

use std::io::{Read, Write};

use pstart_core::limits::MAX_FRAME_LEN;

use crate::error::IpcError;
use crate::message::IpcMessage;

const HEADER_LEN: usize = 4;

/// Serialize one message into a complete frame, including the length prefix.
pub fn encode_frame(message: &IpcMessage) -> Result<Vec<u8>, IpcError> {
    let payload = encode_payload(message)?;
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode one complete frame, including the length prefix.
pub fn decode_frame(frame: &[u8]) -> Result<IpcMessage, IpcError> {
    if frame.len() < HEADER_LEN {
        return Err(IpcError::TruncatedHeader {
            actual: frame.len(),
        });
    }
    let declared = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let payload = &frame[HEADER_LEN..];
    if declared == 0 {
        return Err(IpcError::EmptyFrame);
    }
    if declared > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge {
            actual: declared,
            max: MAX_FRAME_LEN,
        });
    }
    if payload.len() != declared {
        return Err(IpcError::TruncatedPayload {
            actual: payload.len(),
            expected: declared,
        });
    }
    decode_payload(payload)
}

/// Serialize a message to a payload without a length prefix.
pub fn encode_payload(message: &IpcMessage) -> Result<Vec<u8>, IpcError> {
    message.validate()?;
    let payload = postcard::to_allocvec(message).map_err(IpcError::Encode)?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge {
            actual: payload.len(),
            max: MAX_FRAME_LEN,
        });
    }
    Ok(payload)
}

/// Deserialize and validate a payload without a length prefix.
pub fn decode_payload(payload: &[u8]) -> Result<IpcMessage, IpcError> {
    if payload.is_empty() {
        return Err(IpcError::EmptyFrame);
    }
    let (message, remainder): (IpcMessage, &[u8]) =
        postcard::take_from_bytes(payload).map_err(IpcError::Decode)?;
    if !remainder.is_empty() {
        return Err(IpcError::InvalidMessage(
            "payload has trailing bytes".to_owned(),
        ));
    }
    message.validate()?;
    Ok(message)
}

/// Write one framed message to `writer`.
pub fn write_frame<W: Write>(writer: &mut W, message: &IpcMessage) -> Result<(), IpcError> {
    let frame = encode_frame(message)?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

/// Read one framed message from `reader`.
///
/// `max_frame_len` may lower, but never raise, the global [`MAX_FRAME_LEN`].
pub fn read_frame<R: Read>(reader: &mut R, max_frame_len: usize) -> Result<IpcMessage, IpcError> {
    let payload = read_payload(reader, max_frame_len)?;
    decode_payload(&payload)
}

/// Read one payload from `reader`, enforcing the frame limit before allocation.
pub fn read_payload<R: Read>(reader: &mut R, max_frame_len: usize) -> Result<Vec<u8>, IpcError> {
    let limit = effective_frame_limit(max_frame_len);
    let mut header = [0u8; HEADER_LEN];
    let read = read_exact_count(reader, &mut header)?;
    if read == 0 {
        return Err(IpcError::Closed);
    }
    if read != HEADER_LEN {
        return Err(IpcError::TruncatedHeader { actual: read });
    }
    let declared = u32::from_le_bytes(header) as usize;
    if declared == 0 {
        return Err(IpcError::EmptyFrame);
    }
    if declared > limit {
        return Err(IpcError::FrameTooLarge {
            actual: declared,
            max: limit,
        });
    }

    let mut payload = vec![0u8; declared];
    let read = read_exact_count(reader, &mut payload)?;
    if read != declared {
        return Err(IpcError::TruncatedPayload {
            actual: read,
            expected: declared,
        });
    }
    Ok(payload)
}

fn read_exact_count<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<usize, IpcError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(IpcError::Io(err)),
        }
    }
    Ok(filled)
}

pub(crate) const fn effective_frame_limit(requested: usize) -> usize {
    if requested < MAX_FRAME_LEN {
        requested
    } else {
        MAX_FRAME_LEN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pstart_core::limits::{MAX_QUERY_CHARS, MAX_RESULTS};
    use pstart_core::{FileId, FileItem, Frn, VolumeId};
    use std::io::Cursor;

    fn sample_item(index: u64) -> FileItem {
        FileItem {
            id: FileId::new(VolumeId::new("C:").unwrap(), Frn::new(index)),
            name: format!("file-{index}.txt"),
            path: format!("C:\\data\\file-{index}.txt"),
            attributes: 0,
            is_dir: false,
        }
    }

    fn sample_messages() -> Vec<IpcMessage> {
        vec![
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::HelloAck {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                server_version: "0.1.0".to_owned(),
                session_id: 7,
            },
            IpcMessage::Ping { seq: 1 },
            IpcMessage::Pong { seq: 1 },
            IpcMessage::Query {
                request_id: 9,
                text: "报告".to_owned(),
                limit: 50,
            },
            IpcMessage::QueryResult {
                request_id: 9,
                items: vec![sample_item(1), sample_item(2)],
                truncated: false,
            },
            IpcMessage::UsnSyncStatus {
                volume_id: "C:".to_owned(),
                indexed_files: 100,
                is_syncing: true,
            },
            IpcMessage::Error {
                request_id: Some(9),
                code: 4,
            },
        ]
    }

    #[test]
    fn every_variant_roundtrips() {
        for message in sample_messages() {
            let frame = encode_frame(&message).unwrap();
            let back = decode_frame(&frame).unwrap();
            assert_eq!(message, back);
        }
    }

    #[test]
    fn length_prefix_is_little_endian() {
        let message = IpcMessage::Ping { seq: 1 };
        let frame = encode_frame(&message).unwrap();
        let declared = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(declared, frame.len() - 4);
    }

    #[test]
    fn write_and_read_frame_roundtrip() {
        let message = IpcMessage::Query {
            request_id: 3,
            text: "test".to_owned(),
            limit: 10,
        };
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &message).unwrap();
        let mut cursor = Cursor::new(buffer);
        let back = read_frame(&mut cursor, MAX_FRAME_LEN).unwrap();
        assert_eq!(message, back);
    }

    #[test]
    fn empty_frame_is_rejected() {
        let frame = [0u8; 4];
        assert!(matches!(decode_frame(&frame), Err(IpcError::EmptyFrame)));
    }

    #[test]
    fn truncated_header_is_rejected() {
        let result = decode_frame(&[1, 2]);
        assert!(matches!(
            result,
            Err(IpcError::TruncatedHeader { actual: 2 })
        ));
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let mut frame = vec![5, 0, 0, 0];
        frame.extend_from_slice(&[1, 2]);
        assert!(matches!(
            decode_frame(&frame),
            Err(IpcError::TruncatedPayload {
                actual: 2,
                expected: 5
            })
        ));
    }

    #[test]
    fn oversized_declared_length_is_rejected_before_read() {
        let mut header = u32::try_from(MAX_FRAME_LEN + 1)
            .unwrap()
            .to_le_bytes()
            .to_vec();
        header.extend_from_slice(&[0u8; 8]);
        let mut cursor = Cursor::new(header);
        assert!(matches!(
            read_payload(&mut cursor, MAX_FRAME_LEN),
            Err(IpcError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn configured_limit_cannot_exceed_global_limit() {
        let mut header = u32::try_from(MAX_FRAME_LEN + 1)
            .unwrap()
            .to_le_bytes()
            .to_vec();
        header.extend_from_slice(&[0u8; 8]);
        let mut cursor = Cursor::new(header);
        assert!(matches!(
            read_payload(&mut cursor, usize::MAX),
            Err(IpcError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn eof_is_transport_error() {
        let mut cursor = Cursor::new(Vec::new());
        assert!(matches!(
            read_payload(&mut cursor, MAX_FRAME_LEN),
            Err(IpcError::Closed)
        ));
    }

    #[test]
    fn query_too_long_is_rejected_on_encode() {
        let message = IpcMessage::Query {
            request_id: 1,
            text: "a".repeat(MAX_QUERY_CHARS + 1),
            limit: 10,
        };
        assert!(matches!(
            encode_payload(&message),
            Err(IpcError::InvalidMessage(_))
        ));
    }

    #[test]
    fn result_limit_is_enforced_on_encode() {
        let items = (0..=u64::from(MAX_RESULTS)).map(sample_item).collect();
        let message = IpcMessage::QueryResult {
            request_id: 1,
            items,
            truncated: true,
        };
        assert!(matches!(
            encode_payload(&message),
            Err(IpcError::InvalidMessage(_))
        ));
    }

    #[test]
    fn result_limit_is_enforced_on_decode() {
        let items = (0..=u64::from(MAX_RESULTS)).map(sample_item).collect();
        let message = IpcMessage::QueryResult {
            request_id: 1,
            items,
            truncated: true,
        };
        // Bypass encode validation to model a hostile peer.
        let payload = postcard::to_allocvec(&message).unwrap();
        assert!(matches!(decode_payload(&payload), Err(IpcError::Decode(_))));
    }

    #[test]
    fn invalid_limit_is_rejected() {
        for limit in [0, MAX_RESULTS + 1] {
            let message = IpcMessage::Query {
                request_id: 1,
                text: "x".to_owned(),
                limit,
            };
            assert!(matches!(
                encode_payload(&message),
                Err(IpcError::InvalidMessage(_))
            ));
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let frame = encode_frame(&IpcMessage::Ping { seq: 1 }).unwrap();
        let mut extended = frame;
        extended.push(0);
        assert!(decode_frame(&extended).is_err());
    }

    #[test]
    fn arbitrary_bytes_do_not_panic() {
        // Deterministic edge cases; proptest coverage lives in the integration test.
        let samples: &[&[u8]] = &[
            &[],
            &[0],
            &[0, 0, 0, 0],
            &[0xff, 0xff, 0xff, 0xff],
            &[1, 0, 0, 0, 0xff],
            &[4, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef],
        ];
        for sample in samples {
            let _ = decode_frame(sample);
            let mut cursor = Cursor::new(sample.to_vec());
            let _ = read_payload(&mut cursor, MAX_FRAME_LEN);
        }
    }

    #[test]
    fn empty_payload_is_rejected() {
        assert!(matches!(decode_payload(&[]), Err(IpcError::EmptyFrame)));
    }

    #[test]
    fn trailing_payload_bytes_are_rejected() {
        let mut payload = encode_payload(&IpcMessage::Ping { seq: 1 }).unwrap();
        payload.push(0);
        assert!(matches!(
            decode_payload(&payload),
            Err(IpcError::InvalidMessage(_))
        ));
    }

    #[test]
    fn framed_trailing_bytes_are_rejected() {
        let mut payload = encode_payload(&IpcMessage::Ping { seq: 1 }).unwrap();
        payload.push(0);
        let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&payload);
        assert!(matches!(
            decode_frame(&frame),
            Err(IpcError::InvalidMessage(_))
        ));
    }

    #[test]
    fn read_payload_detects_truncated_body() {
        let mut bytes = 5u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[1, 2]);
        let mut cursor = Cursor::new(bytes);
        assert!(matches!(
            read_payload(&mut cursor, MAX_FRAME_LEN),
            Err(IpcError::TruncatedPayload {
                actual: 2,
                expected: 5
            })
        ));
    }

    struct InterruptOnce {
        data: Vec<u8>,
        position: usize,
        interrupted: bool,
    }

    impl Read for InterruptOnce {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "try again",
                ));
            }
            let remaining = &self.data[self.position..];
            let count = remaining.len().min(buffer.len());
            buffer[..count].copy_from_slice(&remaining[..count]);
            self.position += count;
            Ok(count)
        }
    }

    #[test]
    fn interrupted_reads_are_retried() {
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.push(0x2a);
        let mut reader = InterruptOnce {
            data: bytes,
            position: 0,
            interrupted: false,
        };
        assert_eq!(
            read_payload(&mut reader, MAX_FRAME_LEN).unwrap(),
            vec![0x2a]
        );
    }

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("read failed"))
        }
    }

    #[test]
    fn read_errors_are_classified_as_transport() {
        let mut reader = FailingReader;
        assert!(matches!(
            read_payload(&mut reader, MAX_FRAME_LEN),
            Err(IpcError::Io(_))
        ));
    }

    #[test]
    fn oversized_serialized_payload_is_rejected() {
        let path = "报".repeat(pstart_core::MAX_PATH_UTF16);
        let items = (0..u64::from(MAX_RESULTS))
            .map(|index| FileItem {
                id: FileId::new(VolumeId::new("C:").unwrap(), Frn::new(index)),
                name: format!("file-{index}.txt"),
                path: path.clone(),
                attributes: 0,
                is_dir: false,
            })
            .collect();
        let message = IpcMessage::QueryResult {
            request_id: 1,
            items,
            truncated: true,
        };
        assert!(matches!(
            encode_payload(&message),
            Err(IpcError::FrameTooLarge { .. })
        ));
    }
}
