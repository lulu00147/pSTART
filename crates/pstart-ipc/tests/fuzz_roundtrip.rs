use proptest::prelude::*;
use pstart_core::limits::MAX_FRAME_LEN;
use pstart_ipc::{decode_frame, read_payload};
use std::io::Cursor;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_frame_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..8192)) {
        let _ = decode_frame(&bytes);
    }

    #[test]
    fn arbitrary_payload_lengths_never_over_allocate(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        // Treat arbitrary bytes as a stream to exercise the length gate.
        let mut cursor = Cursor::new(bytes);
        let _ = read_payload(&mut cursor, MAX_FRAME_LEN);
    }
}
