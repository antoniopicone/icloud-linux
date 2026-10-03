//! Apple's "topotext" CRDT documents, as used by `TitleDocument` and
//! `NotesDocument` on Reminders records (and by Notes bodies).
//!
//! On the wire a document is base64 text of a zlib stream of a protobuf
//! `versioned_document.Document`, whose first version carries a
//! `topotext.String`. Only the plain text is read back; formatting runs,
//! attachments and the merge history are ignored.
//!
//! [`encode`] is a port of `pyicloud`'s `_encode_crdt_document` and produces
//! the same protobuf bytes (checked against vectors captured from pyicloud
//! 2.7.0 in `tests/fixtures/pyicloud_reminders.json`). The zlib framing may
//! differ byte for byte from Python's, which is harmless: any zlib reader,
//! Apple's included, inflates both to the same document.
//!
//! The schema is proto2, so every field that the encoder sets is written even
//! when it holds zero, and `Substring.child` is a non-packed repeated field.
//! Both details matter for byte equality and are why each field below is an
//! `Option` and `child` is marked `packed = "false"`.

use std::io::{Read as _, Write as _};

use base64::{
    Engine as _, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use flate2::{
    Compression,
    read::{GzDecoder, ZlibDecoder},
    write::ZlibEncoder,
};
use prost::Message;

use crate::error::{Error, Result};

/// The replica UUID pyicloud writes into every document it creates.
const REPLICA_UUID: [u8; 16] =
    [0xd4, 0x6b, 0xca, 0xe4, 0x1b, 0x87, 0x66, 0xc1, 0x8d, 0x75, 0xef, 0xe3, 0x5c, 0x91, 0x45, 0xc3];
const CLOCK_MAX: u32 = u32::MAX;

/// Base64 that accepts the encoded text with or without its `=` padding,
/// the way pyicloud pads before decoding.
const LENIENT_B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub(crate) fn decode_base64(text: &str) -> Option<Vec<u8>> {
    LENIENT_B64.decode(text.trim()).ok()
}

// ---- schema ------------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
struct Document {
    #[prost(uint32, optional, tag = "1")]
    serialization_version: Option<u32>,
    #[prost(message, repeated, tag = "2")]
    version: Vec<Version>,
}

#[derive(Clone, PartialEq, Message)]
struct Version {
    #[prost(uint32, optional, tag = "1")]
    serialization_version: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    minimum_supported_version: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "3")]
    data: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct TopoString {
    #[prost(string, optional, tag = "2")]
    string: Option<String>,
    #[prost(message, repeated, tag = "3")]
    substring: Vec<Substring>,
    #[prost(message, optional, tag = "4")]
    timestamp: Option<VectorTimestamp>,
    #[prost(message, repeated, tag = "5")]
    attribute_run: Vec<AttributeRun>,
}

#[derive(Clone, PartialEq, Message)]
struct VectorTimestamp {
    #[prost(message, repeated, tag = "1")]
    clock: Vec<Clock>,
}

#[derive(Clone, PartialEq, Message)]
struct Clock {
    #[prost(bytes = "vec", optional, tag = "1")]
    replica_uuid: Option<Vec<u8>>,
    #[prost(message, repeated, tag = "2")]
    replica_clock: Vec<ReplicaClock>,
}

#[derive(Clone, PartialEq, Message)]
struct ReplicaClock {
    #[prost(uint32, optional, tag = "1")]
    clock: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    subclock: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
struct CharId {
    #[prost(uint32, optional, tag = "1")]
    replica_id: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    clock: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
struct Substring {
    #[prost(message, optional, tag = "1")]
    char_id: Option<CharId>,
    #[prost(uint32, optional, tag = "2")]
    length: Option<u32>,
    #[prost(message, optional, tag = "3")]
    timestamp: Option<CharId>,
    #[prost(bool, optional, tag = "4")]
    tombstone: Option<bool>,
    #[prost(uint32, repeated, packed = "false", tag = "5")]
    child: Vec<u32>,
}

/// Only the length is modelled; every other attribute is skipped on decode
/// and never written.
#[derive(Clone, PartialEq, Message)]
struct AttributeRun {
    #[prost(uint32, optional, tag = "1")]
    length: Option<u32>,
}

fn char_id(replica_id: u32, clock: u32) -> CharId {
    CharId { replica_id: Some(replica_id), clock: Some(clock) }
}

// ---- encode ------------------------------------------------------------------

/// The `topotext.String` protobuf bytes for `text`, before versioning and
/// compression.
fn string_bytes(text: &str) -> Vec<u8> {
    // Apple measures topotext offsets in UTF-16 code units, not characters.
    let len = u32::try_from(text.encode_utf16().count()).unwrap_or(u32::MAX);

    let mut substring = vec![Substring {
        char_id: Some(char_id(0, 0)),
        length: Some(0),
        timestamp: Some(char_id(0, 0)),
        tombstone: None,
        child: vec![1],
    }];
    if len > 0 {
        substring.push(Substring {
            char_id: Some(char_id(1, 0)),
            length: Some(len),
            timestamp: Some(char_id(1, 0)),
            tombstone: None,
            child: vec![2],
        });
    }
    substring.push(Substring {
        char_id: Some(char_id(0, CLOCK_MAX)),
        length: Some(0),
        timestamp: Some(char_id(0, CLOCK_MAX)),
        tombstone: None,
        child: Vec::new(),
    });

    let value = TopoString {
        string: Some(text.to_owned()),
        substring,
        timestamp: Some(VectorTimestamp {
            clock: vec![Clock {
                replica_uuid: Some(REPLICA_UUID.to_vec()),
                replica_clock: vec![
                    ReplicaClock { clock: Some(len), subclock: None },
                    ReplicaClock { clock: Some(1), subclock: None },
                ],
            }],
        }),
        attribute_run: if len > 0 { vec![AttributeRun { length: Some(len) }] } else { Vec::new() },
    };
    value.encode_to_vec()
}

/// The uncompressed `versioned_document.Document` bytes for `text`.
pub(crate) fn document_bytes(text: &str) -> Vec<u8> {
    Document {
        serialization_version: Some(0),
        version: vec![Version {
            serialization_version: Some(0),
            minimum_supported_version: Some(0),
            data: Some(string_bytes(text)),
        }],
    }
    .encode_to_vec()
}

/// Encode `text` as a fresh CRDT document, ready for a `STRING` field.
pub fn encode(text: &str) -> String {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    // Writing into a Vec cannot fail.
    let _ = encoder.write_all(&document_bytes(text));
    let compressed = encoder.finish().unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(compressed)
}

// ---- decode ------------------------------------------------------------------

/// Read the plain text out of a CRDT document.
///
/// Accepts the forms pyicloud accepts, in the same order: a versioned
/// document, a bare version, then a bare `topotext.String`; each zlib- or
/// gzip-compressed or not compressed at all.
pub fn decode(payload: &[u8]) -> Result<String> {
    let data = inflate(payload);

    if let Ok(document) = Document::decode(data.as_slice())
        && let Some(first) = document.version.first()
        && let Ok(value) = TopoString::decode(first.data.as_deref().unwrap_or_default())
    {
        return Ok(value.string.unwrap_or_default());
    }
    if let Ok(version) = Version::decode(data.as_slice())
        && let Some(bytes) = version.data.as_deref().filter(|b| !b.is_empty())
        && let Ok(value) = TopoString::decode(bytes)
    {
        return Ok(value.string.unwrap_or_default());
    }
    if let Ok(value) = TopoString::decode(data.as_slice())
        && let Some(text) = value.string.filter(|s| !s.is_empty())
    {
        return Ok(text);
    }
    Err(Error::Protocol("unable to decode a CRDT text document".into()))
}

/// [`decode`] for the base64 text found in a record field.
pub fn decode_wire(text: &str) -> Result<String> {
    let bytes = decode_base64(text).ok_or_else(|| Error::Protocol("CRDT document is not valid base64".into()))?;
    decode(&bytes)
}

fn inflate(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if ZlibDecoder::new(payload).read_to_end(&mut out).is_ok() {
        return out;
    }
    out.clear();
    if GzDecoder::new(payload).read_to_end(&mut out).is_ok() {
        return out;
    }
    payload.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }

    #[test]
    fn round_trips_plain_accented_and_astral_text() {
        for text in ["", "Comprare il latte", "Café ☕ 🎉 ok", &"a".repeat(300)] {
            assert_eq!(decode_wire(&encode(text)).unwrap(), text);
        }
    }

    /// The protobuf bytes equal pyicloud 2.7.0's for the recorded texts,
    /// which include an empty one, accents, an astral emoji and a long one
    /// (multi-byte varint lengths).
    #[test]
    fn documents_match_pyicloud_byte_for_byte() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/pyicloud_reminders.json")).unwrap();
        let vectors = fixture["crdt_vectors"].as_array().unwrap();
        assert_eq!(vectors.len(), 4);
        for vector in vectors {
            let text = vector["text"].as_str().unwrap();
            let ours = hex(&document_bytes(text));
            assert_eq!(ours, vector["document_hex"].as_str().unwrap(), "document for {text:?}");
            // And what pyicloud wrote decodes back to the text.
            assert_eq!(decode_wire(vector["wire"].as_str().unwrap()).unwrap(), text);
        }
    }

    #[test]
    fn lengths_are_counted_in_utf16_units() {
        // "🎉" is one character but two UTF-16 units.
        let bytes = string_bytes("🎉");
        let value = TopoString::decode(bytes.as_slice()).unwrap();
        assert_eq!(value.substring[1].length, Some(2));
        assert_eq!(value.attribute_run[0].length, Some(2));
    }

    #[test]
    fn a_bare_uncompressed_string_still_decodes() {
        let bytes = TopoString { string: Some("legacy".into()), ..Default::default() }.encode_to_vec();
        assert_eq!(decode(&bytes).unwrap(), "legacy");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(decode(&[0xff, 0x00, 0x13]).is_err());
        assert!(decode_wire("!!not base64!!").is_err());
    }
}
