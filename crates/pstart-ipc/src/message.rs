//! Wire messages.
//!
//! The enum discriminant is serialized by postcard as a stable variant index.
//! Never reorder existing variants; append new variants at the end.

use pstart_core::limits::{
    MAX_CLIENT_VERSION_LEN, MAX_QUERY_BYTES, MAX_QUERY_CHARS, MAX_RESULTS, MAX_SERVER_VERSION_LEN,
};
use pstart_core::{FileItem, VolumeId};
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::marker::PhantomData;

use crate::error::IpcError;

/// A complete IPC protocol message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcMessage {
    /// First client message; requests protocol version negotiation.
    Hello {
        /// Wire protocol version requested by the client.
        protocol_version: u16,
        /// Human-readable client version. Not trusted for authorization.
        client_version: String,
    },
    /// Server accepted the protocol version.
    HelloAck {
        /// Negotiated protocol version.
        protocol_version: u16,
        /// Human-readable server version.
        server_version: String,
        /// Opaque session identifier for log correlation.
        session_id: u64,
    },
    /// Liveness probe.
    Ping {
        /// Monotonic client sequence number.
        seq: u64,
    },
    /// Liveness response.
    Pong {
        /// Echoed sequence number.
        seq: u64,
    },
    /// Search request.
    Query {
        /// Client-assigned request identifier.
        request_id: u64,
        /// Search text.
        text: String,
        /// Maximum requested result count.
        limit: u32,
    },
    /// Search response.
    QueryResult {
        /// Echoed request identifier.
        request_id: u64,
        /// Bounded result list.
        #[serde(deserialize_with = "deserialize_bounded_items")]
        items: Vec<FileItem>,
        /// Whether the server stopped early because of a result or time budget.
        truncated: bool,
    },
    /// Index synchronization status for one volume.
    UsnSyncStatus {
        /// Volume identifier.
        volume_id: String,
        /// Number of indexed files.
        indexed_files: u64,
        /// Whether synchronization is currently active.
        is_syncing: bool,
    },
    /// Stable error response.
    Error {
        /// Correlated request identifier, when available.
        request_id: Option<u64>,
        /// Stable [`pstart_core::ErrorCode`] value.
        code: u32,
    },
    /// Request a one-volume status snapshot.
    StatusRequest {
        /// Client-assigned request identifier.
        request_id: u64,
    },
    /// Correlated status response.
    StatusResult {
        /// Echoed request identifier.
        request_id: u64,
        /// Volume identifier.
        volume_id: String,
        /// Number of indexed files.
        indexed_files: u64,
        /// Whether synchronization is currently active.
        is_syncing: bool,
    },
}

impl IpcMessage {
    /// Validate hard limits and semantic constraints before encoding/use.
    pub fn validate(&self) -> Result<(), IpcError> {
        match self {
            Self::Hello {
                client_version,
                protocol_version: _,
            } => validate_bounded_text(
                "client_version",
                client_version,
                MAX_CLIENT_VERSION_LEN,
                false,
            ),
            Self::HelloAck {
                server_version,
                protocol_version: _,
                session_id: _,
            } => validate_bounded_text(
                "server_version",
                server_version,
                MAX_SERVER_VERSION_LEN,
                false,
            ),
            Self::Ping { .. } | Self::Pong { .. } => Ok(()),
            Self::Query { text, limit, .. } => {
                if !query_limit_in_range(*limit) {
                    return Err(IpcError::InvalidMessage(format!(
                        "query limit {limit} is outside 1..={MAX_RESULTS}"
                    )));
                }
                if !query_bytes_in_range(text.len()) {
                    return Err(IpcError::InvalidMessage(format!(
                        "query exceeds {MAX_QUERY_BYTES} bytes"
                    )));
                }
                if text.contains('\0') {
                    return Err(IpcError::InvalidMessage(
                        "query text contains NUL".to_owned(),
                    ));
                }
                if !query_chars_in_range(text.chars().count()) {
                    return Err(IpcError::InvalidMessage(format!(
                        "query exceeds {MAX_QUERY_CHARS} characters"
                    )));
                }
                Ok(())
            }
            Self::QueryResult { items, .. } => {
                if !result_count_in_range(items.len()) {
                    return Err(IpcError::InvalidMessage(format!(
                        "result list has {} items, maximum is {MAX_RESULTS}",
                        items.len()
                    )));
                }
                for item in items {
                    item.validate()
                        .map_err(|err| IpcError::InvalidMessage(err.to_string()))?;
                }
                Ok(())
            }
            Self::UsnSyncStatus { volume_id, .. } => VolumeId::new(volume_id.clone())
                .map(|_| ())
                .map_err(|err| IpcError::InvalidMessage(err.to_string())),
            Self::Error { .. } => Ok(()),
            Self::StatusRequest { .. } => Ok(()),
            Self::StatusResult { volume_id, .. } => VolumeId::new(volume_id.clone())
                .map(|_| ())
                .map_err(|err| IpcError::InvalidMessage(err.to_string())),
        }
    }

    /// Stable, low-cardinality message kind for logs and tests.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::HelloAck { .. } => "hello_ack",
            Self::Ping { .. } => "ping",
            Self::Pong { .. } => "pong",
            Self::Query { .. } => "query",
            Self::QueryResult { .. } => "query_result",
            Self::UsnSyncStatus { .. } => "usn_sync_status",
            Self::Error { .. } => "error",
            Self::StatusRequest { .. } => "status_request",
            Self::StatusResult { .. } => "status_result",
        }
    }
}

fn validate_bounded_text(
    field: &str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), IpcError> {
    if !allow_empty && value.is_empty() {
        return Err(IpcError::InvalidMessage(format!("{field} is empty")));
    }
    if value.len() > max_bytes {
        return Err(IpcError::InvalidMessage(format!(
            "{field} is {} bytes, maximum is {max_bytes}",
            value.len()
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(IpcError::InvalidMessage(format!(
            "{field} contains control characters"
        )));
    }
    Ok(())
}

fn deserialize_bounded_items<'de, D>(deserializer: D) -> Result<Vec<FileItem>, D::Error>
where
    D: Deserializer<'de>,
{
    struct BoundedItems(PhantomData<FileItem>);

    impl<'de> Visitor<'de> for BoundedItems {
        type Value = Vec<FileItem>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {MAX_RESULTS} file result items")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let hint = seq.size_hint().unwrap_or(0).min(MAX_RESULTS as usize);
            let mut items = Vec::with_capacity(hint);
            while let Some(item) = seq.next_element::<FileItem>()? {
                if items.len() >= MAX_RESULTS as usize {
                    return Err(de::Error::custom(format!(
                        "file result list exceeds {MAX_RESULTS} items"
                    )));
                }
                items.push(item);
            }
            Ok(items)
        }
    }

    deserializer.deserialize_seq(BoundedItems(PhantomData))
}

pub(crate) const fn query_limit_in_range(limit: u32) -> bool {
    limit != 0 && limit <= MAX_RESULTS
}

pub(crate) const fn query_bytes_in_range(bytes: usize) -> bool {
    bytes <= MAX_QUERY_BYTES
}

pub(crate) const fn query_chars_in_range(chars: usize) -> bool {
    chars <= MAX_QUERY_CHARS
}

pub(crate) const fn result_count_in_range(count: usize) -> bool {
    count <= MAX_RESULTS as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use pstart_core::{FileId, Frn};

    fn item(name: &str) -> FileItem {
        FileItem {
            id: FileId::new(VolumeId::new("C:").unwrap(), Frn::new(1)),
            name: name.into(),
            path: format!("C:\\{name}"),
            attributes: 0,
            is_dir: false,
        }
    }

    #[test]
    fn message_kinds_are_stable() {
        let messages = [
            (
                IpcMessage::Hello {
                    protocol_version: pstart_core::PROTOCOL_VERSION,
                    client_version: "0.1.0".to_owned(),
                },
                "hello",
            ),
            (
                IpcMessage::HelloAck {
                    protocol_version: pstart_core::PROTOCOL_VERSION,
                    server_version: "0.1.0".to_owned(),
                    session_id: 1,
                },
                "hello_ack",
            ),
            (IpcMessage::Ping { seq: 1 }, "ping"),
            (IpcMessage::Pong { seq: 1 }, "pong"),
            (
                IpcMessage::Query {
                    request_id: 1,
                    text: "x".to_owned(),
                    limit: 1,
                },
                "query",
            ),
            (
                IpcMessage::QueryResult {
                    request_id: 1,
                    items: vec![],
                    truncated: false,
                },
                "query_result",
            ),
            (
                IpcMessage::UsnSyncStatus {
                    volume_id: "C:".to_owned(),
                    indexed_files: 0,
                    is_syncing: false,
                },
                "usn_sync_status",
            ),
            (
                IpcMessage::Error {
                    request_id: None,
                    code: 1,
                },
                "error",
            ),
            (
                IpcMessage::StatusRequest { request_id: 1 },
                "status_request",
            ),
            (
                IpcMessage::StatusResult {
                    request_id: 1,
                    volume_id: "C:".to_owned(),
                    indexed_files: 0,
                    is_syncing: false,
                },
                "status_result",
            ),
        ];
        for (message, expected) in messages {
            assert_eq!(message.kind(), expected);
        }
    }

    #[test]
    fn query_byte_limit_is_enforced() {
        let too_long = IpcMessage::Query {
            request_id: 1,
            text: "a".repeat(MAX_QUERY_BYTES + 1),
            limit: 1,
        };
        assert!(matches!(
            too_long.validate(),
            Err(IpcError::InvalidMessage(_))
        ));

        let boundary = IpcMessage::Query {
            request_id: 1,
            text: "😀".repeat(MAX_QUERY_CHARS),
            limit: 1,
        };
        assert!(boundary.validate().is_ok());
    }

    #[test]
    fn query_with_nul_is_rejected() {
        assert!(matches!(
            IpcMessage::Query {
                request_id: 1,
                text: "a\0b".to_owned(),
                limit: 1,
            }
            .validate(),
            Err(IpcError::InvalidMessage(_))
        ));
    }

    #[test]
    fn hello_version_bounds_are_enforced() {
        assert!(matches!(
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: String::new(),
            }
            .validate(),
            Err(IpcError::InvalidMessage(_))
        ));
        assert!(matches!(
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "x".repeat(MAX_CLIENT_VERSION_LEN + 1),
            }
            .validate(),
            Err(IpcError::InvalidMessage(_))
        ));
        assert!(matches!(
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "bad\nversion".to_owned(),
            }
            .validate(),
            Err(IpcError::InvalidMessage(_))
        ));
        assert!(matches!(
            IpcMessage::HelloAck {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                server_version: "x".repeat(MAX_SERVER_VERSION_LEN + 1),
                session_id: 1,
            }
            .validate(),
            Err(IpcError::InvalidMessage(_))
        ));
    }

    #[test]
    fn usn_status_validates_volume_id() {
        assert!(IpcMessage::UsnSyncStatus {
            volume_id: "C:".to_owned(),
            indexed_files: 1,
            is_syncing: true,
        }
        .validate()
        .is_ok());
        assert!(matches!(
            IpcMessage::UsnSyncStatus {
                volume_id: String::new(),
                indexed_files: 1,
                is_syncing: true,
            }
            .validate(),
            Err(IpcError::InvalidMessage(_))
        ));
        assert!(IpcMessage::StatusResult {
            request_id: 1,
            volume_id: "C:".to_owned(),
            indexed_files: 1,
            is_syncing: false,
        }
        .validate()
        .is_ok());
        assert!(matches!(
            IpcMessage::StatusResult {
                request_id: 1,
                volume_id: String::new(),
                indexed_files: 1,
                is_syncing: false,
            }
            .validate(),
            Err(IpcError::InvalidMessage(_))
        ));
    }

    #[test]
    fn query_result_items_are_validated() {
        assert!(IpcMessage::QueryResult {
            request_id: 1,
            items: vec![item("ok.txt")],
            truncated: false,
        }
        .validate()
        .is_ok());
        assert!(matches!(
            IpcMessage::QueryResult {
                request_id: 1,
                items: vec![item(&"x".repeat(256))],
                truncated: false,
            }
            .validate(),
            Err(IpcError::InvalidMessage(_))
        ));

        let mut long_path = item("ok.txt");
        long_path.path = "C:\\".to_owned() + &"x".repeat(pstart_core::MAX_PATH_UTF16);
        assert!(matches!(
            IpcMessage::QueryResult {
                request_id: 1,
                items: vec![long_path],
                truncated: false,
            }
            .validate(),
            Err(IpcError::InvalidMessage(_))
        ));
    }

    #[test]
    fn ping_pong_and_error_are_valid() {
        assert!(IpcMessage::Ping { seq: 1 }.validate().is_ok());
        assert!(IpcMessage::Pong { seq: 1 }.validate().is_ok());
        assert!(IpcMessage::Error {
            request_id: Some(1),
            code: 999,
        }
        .validate()
        .is_ok());
    }

    #[allow(dead_code)] // Variants preserve the postcard discriminant of IpcMessage.
    #[derive(serde::Serialize)]
    enum MirrorQueryResult {
        V0,
        V1,
        V2,
        V3,
        V4,
        V5 {
            request_id: u64,
            items: u8,
            truncated: bool,
        },
    }

    #[test]
    fn wrong_items_shape_uses_visitor_expectation() {
        let payload = postcard::to_allocvec(&MirrorQueryResult::V5 {
            request_id: 1,
            items: 7,
            truncated: false,
        })
        .unwrap();
        let error = crate::frame::decode_payload(&payload).unwrap_err();
        assert!(matches!(error, IpcError::Decode(_)));
    }

    #[test]
    fn visitor_expectation_is_reachable_directly() {
        use serde::de::value::{Error as ValueError, StrDeserializer};
        use serde::de::IntoDeserializer;

        let deserializer: StrDeserializer<ValueError> = "not-a-sequence".into_deserializer();
        let result: Result<Vec<FileItem>, ValueError> = deserialize_bounded_items(deserializer);
        let error = result.unwrap_err().to_string();
        assert!(error.contains("at most"), "{error}");
    }
}
