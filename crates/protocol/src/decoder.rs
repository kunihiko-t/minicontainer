use minios_abi::control::{FRAME_HEADER_LEN, FrameHeader, FrameKind};

use crate::ProtocolError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, ProtocolError> {
        self.buffer.extend_from_slice(bytes);

        let mut frames = Vec::new();
        let mut consumed = 0;

        while self.buffer.len() - consumed >= FRAME_HEADER_LEN {
            let header =
                match FrameHeader::decode(&self.buffer[consumed..consumed + FRAME_HEADER_LEN]) {
                    Ok(header) => header,
                    Err(error) => {
                        self.buffer.clear();
                        return Err(ProtocolError::Header(error));
                    }
                };
            let payload_len = header.payload_len as usize;
            let frame_len = FRAME_HEADER_LEN + payload_len;

            if self.buffer.len() - consumed < frame_len {
                break;
            }

            let payload_start = consumed + FRAME_HEADER_LEN;
            frames.push(Frame {
                kind: header.kind,
                payload: self.buffer[payload_start..payload_start + payload_len].to_vec(),
            });
            consumed += frame_len;
        }

        if consumed > 0 {
            self.buffer.drain(..consumed);
        }

        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoder, Frame};
    use minios_abi::control::{FrameKind, ReadyPayload};

    // Production break caught: retaining only the current UART chunk instead of
    // buffering it loses a frame split at any byte boundary.
    #[test]
    fn decodes_a_frame_one_byte_at_a_time() {
        let encoded = encode_test_frame(FrameKind::Stdout, b"hello");
        let mut decoder = Decoder::new();
        let mut frames = Vec::new();

        for byte in encoded {
            frames.extend(decoder.push(&[byte]).unwrap());
        }

        assert_eq!(
            frames,
            vec![Frame {
                kind: FrameKind::Stdout,
                payload: b"hello".to_vec(),
            }]
        );
    }

    // Production break caught: stopping after the first complete frame drops
    // concatenated UART frames or returns them out of order.
    #[test]
    fn returns_concatenated_frames_in_order() {
        let mut encoded = encode_test_frame(FrameKind::Stdout, b"first");
        encoded.extend_from_slice(&encode_test_frame(FrameKind::Stderr, b"second"));

        assert_eq!(
            Decoder::new().push(&encoded).unwrap(),
            vec![
                Frame {
                    kind: FrameKind::Stdout,
                    payload: b"first".to_vec(),
                },
                Frame {
                    kind: FrameKind::Stderr,
                    payload: b"second".to_vec(),
                },
            ]
        );
    }

    // Production break caught: accepting only a preferred chunk boundary fails
    // when the transport splits the same frame at a different byte.
    #[test]
    fn decodes_a_frame_at_every_two_chunk_split_point() {
        let encoded = encode_test_frame(FrameKind::Diagnostic, b"split points");
        let expected = vec![Frame {
            kind: FrameKind::Diagnostic,
            payload: b"split points".to_vec(),
        }];

        for split in 0..=encoded.len() {
            let mut decoder = Decoder::new();
            let mut frames = decoder.push(&encoded[..split]).unwrap();
            frames.extend(decoder.push(&encoded[split..]).unwrap());
            assert_eq!(frames, expected, "split at byte {split}");
        }
    }

    // Production break caught: interpreting an incomplete header as a complete
    // frame discards the bytes before its payload can arrive.
    #[test]
    fn retains_a_complete_header_until_its_payload_arrives() {
        let encoded = encode_test_frame(FrameKind::Stdout, b"payload");
        let mut decoder = Decoder::new();

        assert_eq!(decoder.push(&encoded[..12]).unwrap(), Vec::<Frame>::new());
        assert_eq!(
            decoder.push(&encoded[12..]).unwrap(),
            vec![Frame {
                kind: FrameKind::Stdout,
                payload: b"payload".to_vec(),
            }]
        );
    }

    // Production break caught: draining an incomplete payload loses its prefix
    // when the rest of the frame arrives later.
    #[test]
    fn retains_an_incomplete_payload_until_complete() {
        let encoded = encode_test_frame(FrameKind::Stderr, b"payload");
        let mut decoder = Decoder::new();

        assert_eq!(decoder.push(&encoded[..14]).unwrap(), Vec::<Frame>::new());
        assert_eq!(
            decoder.push(&encoded[14..]).unwrap(),
            vec![Frame {
                kind: FrameKind::Stderr,
                payload: b"payload".to_vec(),
            }]
        );
    }

    // Production break caught: treating an allowed empty payload as incomplete
    // prevents empty stdout frames from being delivered.
    #[test]
    fn decodes_an_allowed_zero_length_frame() {
        assert_eq!(
            Decoder::new()
                .push(&encode_test_frame(FrameKind::Stdout, b""))
                .unwrap(),
            vec![Frame {
                kind: FrameKind::Stdout,
                payload: Vec::new(),
            }]
        );
    }

    // Production break caught: imposing a lower decoder-local payload cap
    // rejects a payload accepted by the pinned ABI.
    #[test]
    fn decodes_a_64_kib_payload() {
        let payload = vec![0xA5; 64 * 1024];

        assert_eq!(
            Decoder::new()
                .push(&encode_test_frame(FrameKind::Stdout, &payload))
                .unwrap(),
            vec![Frame {
                kind: FrameKind::Stdout,
                payload,
            }]
        );
    }

    // Production break caught: valid fixed-size control payloads are rejected,
    // reordered, or decoded with a layout different from the pinned ABI.
    #[test]
    fn decodes_valid_ready_and_exit_frames_with_exact_payload_bytes() {
        let ready_bytes = [1, 0, 0, 0];
        let exit_bytes = [42, 0, 0, 0];
        let mut encoded = encode_test_frame(FrameKind::Ready, &ready_bytes);
        encoded.extend_from_slice(&encode_test_frame(FrameKind::Exit, &exit_bytes));

        let frames = Decoder::new().push(&encoded).unwrap();

        assert_eq!(
            frames,
            vec![
                Frame {
                    kind: FrameKind::Ready,
                    payload: ready_bytes.to_vec(),
                },
                Frame {
                    kind: FrameKind::Exit,
                    payload: exit_bytes.to_vec(),
                },
            ]
        );
        assert_eq!(
            ReadyPayload::decode(&frames[0].payload),
            Ok(ReadyPayload {
                abi_major: 1,
                abi_minor: 0,
            })
        );
        assert_eq!(
            u32::from_le_bytes(frames[1].payload.as_slice().try_into().unwrap()),
            42
        );
    }

    // Production break caught: skipping ABI header validation accepts an
    // oversized payload declaration and leaves the stream waiting forever.
    #[test]
    fn rejects_a_payload_length_over_64_kib() {
        assert_eq!(
            Decoder::new().push(&encode_test_header(FrameKind::Stdout, 64 * 1024 + 1)),
            Err(crate::ProtocolError::Header(
                minios_abi::control::ControlError::PayloadTooLarge
            ))
        );
    }

    // Production break caught: scanning after a malformed header silently
    // resynchronizes to a later valid frame in the same UART delivery.
    #[test]
    fn rejects_invalid_magic_without_resynchronizing_the_same_push() {
        let mut bytes = encode_test_header(FrameKind::Stdout, 0);
        bytes[0] = b'X';
        bytes.extend_from_slice(&encode_test_frame(FrameKind::Stdout, b"later"));

        assert_eq!(
            Decoder::new().push(&bytes),
            Err(crate::ProtocolError::Header(
                minios_abi::control::ControlError::WrongMagic
            ))
        );
    }

    // Production break caught: retaining malformed bytes after an error makes
    // a valid frame from a later, separate push fail too.
    #[test]
    fn recovers_with_a_valid_frame_in_a_subsequent_push() {
        let mut malformed = encode_test_header(FrameKind::Stdout, 0);
        malformed[0] = b'X';
        let mut decoder = Decoder::new();

        assert_eq!(
            decoder.push(&malformed),
            Err(crate::ProtocolError::Header(
                minios_abi::control::ControlError::WrongMagic
            ))
        );
        assert_eq!(
            decoder
                .push(&encode_test_frame(FrameKind::Stdout, b"recovered"))
                .unwrap(),
            vec![Frame {
                kind: FrameKind::Stdout,
                payload: b"recovered".to_vec(),
            }]
        );
    }

    // Characterization contract: a later malformed header must discard frames
    // already decoded from this push, clear retained bytes, and never scan to
    // a valid frame later in those same bytes.
    #[test]
    fn discards_earlier_complete_frames_when_a_later_header_is_malformed() {
        let mut bytes = encode_test_frame(FrameKind::Stdout, b"discarded");
        let mut malformed = encode_test_header(FrameKind::Stdout, 0);
        malformed[0] = b'X';
        bytes.extend_from_slice(&malformed);
        bytes.extend_from_slice(&encode_test_frame(FrameKind::Stdout, b"not scanned"));
        let mut decoder = Decoder::new();

        assert_eq!(
            decoder.push(&bytes),
            Err(crate::ProtocolError::Header(
                minios_abi::control::ControlError::WrongMagic
            ))
        );
        assert_eq!(
            decoder
                .push(&encode_test_frame(FrameKind::Stdout, b"later"))
                .unwrap(),
            vec![Frame {
                kind: FrameKind::Stdout,
                payload: b"later".to_vec(),
            }]
        );
    }

    // Characterization contract: the decoder delegates Ready's fixed payload
    // length validation to the pinned ABI header decoder.
    #[test]
    fn rejects_ready_with_a_non_four_byte_payload_length() {
        assert_eq!(
            Decoder::new().push(&encode_test_header(FrameKind::Ready, 3)),
            Err(crate::ProtocolError::Header(
                minios_abi::control::ControlError::WrongFixedPayloadLength
            ))
        );
    }

    // Characterization contract: the decoder delegates Exit's fixed payload
    // length validation to the pinned ABI header decoder.
    #[test]
    fn rejects_exit_with_a_non_four_byte_payload_length() {
        assert_eq!(
            Decoder::new().push(&encode_test_header(FrameKind::Exit, 5)),
            Err(crate::ProtocolError::Header(
                minios_abi::control::ControlError::WrongFixedPayloadLength
            ))
        );
    }

    fn encode_test_frame(kind: FrameKind, payload: &[u8]) -> Vec<u8> {
        let mut encoded = encode_test_header(kind, payload.len() as u32);
        encoded.extend_from_slice(payload);
        encoded
    }

    fn encode_test_header(kind: FrameKind, payload_len: u32) -> Vec<u8> {
        let kind = match kind {
            FrameKind::Ready => 1,
            FrameKind::Stdout => 2,
            FrameKind::Stderr => 3,
            FrameKind::Exit => 4,
            FrameKind::GuestError => 5,
            FrameKind::Diagnostic => 6,
        };
        let mut encoded = Vec::with_capacity(12);
        encoded.extend_from_slice(b"MCF1");
        encoded.push(kind);
        encoded.extend_from_slice(&[0, 0, 0]);
        encoded.extend_from_slice(&payload_len.to_le_bytes());
        encoded
    }
}
