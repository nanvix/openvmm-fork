// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Frozen outer protocol for the control-session broker.

use thiserror::Error;

pub const HEADER_LEN: usize = 44;
pub const MAX_DATA_LEN: usize = 65_536;
const MAGIC: [u8; 4] = *b"NVXS";
const VERSION: u16 = 1;

/// A protocol-v1 record type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RecordType {
    GuestAttach = 1,
    HostAttach = 2,
    Reset = 3,
    Ack = 4,
    Data = 5,
    Wait = 6,
    Ready = 7,
    Error = 8,
}

impl TryFrom<u8> for RecordType {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::GuestAttach),
            2 => Ok(Self::HostAttach),
            3 => Ok(Self::Reset),
            4 => Ok(Self::Ack),
            5 => Ok(Self::Data),
            6 => Ok(Self::Wait),
            7 => Ok(Self::Ready),
            8 => Ok(Self::Error),
            _ => Err(ProtocolError::InvalidType(value)),
        }
    }
}

/// A validated protocol-v1 record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    pub record_type: RecordType,
    pub instance_id: [u8; 16],
    pub epoch: u64,
    pub sequence: u64,
    pub payload: Vec<u8>,
}

impl Record {
    /// Creates a bootstrap record. Bootstrap records never consume a sequence.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by protocol tests"))]
    pub fn bootstrap(record_type: RecordType, payload: Vec<u8>) -> Self {
        Self {
            record_type,
            instance_id: [0; 16],
            epoch: 0,
            sequence: 0,
            payload,
        }
    }

    /// Creates a post-bootstrap record.
    pub fn session(
        record_type: RecordType,
        instance_id: [u8; 16],
        epoch: u64,
        sequence: u64,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            record_type,
            instance_id,
            epoch,
            sequence,
            payload,
        }
    }
}

/// Protocol decoding and validation failures.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProtocolError {
    #[error("invalid control-session magic")]
    InvalidMagic,
    #[error("unsupported control-session version {0}")]
    InvalidVersion(u16),
    #[error("unknown control-session record type {0}")]
    InvalidType(u8),
    #[error("control-session flags must be zero, got {0}")]
    InvalidFlags(u8),
    #[error("invalid payload length {length} for {record_type:?}")]
    InvalidPayloadLength {
        record_type: RecordType,
        length: u32,
    },
    #[error("invalid control-session error code {0}")]
    InvalidErrorCode(u32),
    #[error("bootstrap record contains session identity")]
    InvalidBootstrapIdentity,
    #[error("invalid parser snapshot: {0}")]
    InvalidSnapshot(&'static str),
    #[error("encoded record length overflows the platform size")]
    LengthOverflow,
}

/// Serializable-neutral state for an incremental parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParserSnapshot {
    pub header_bytes: Vec<u8>,
    pub header_count: usize,
    pub body_bytes: Vec<u8>,
    pub declared_body_len: Option<u32>,
}

/// The result of accepting bytes into a parser.
#[derive(Debug, Eq, PartialEq)]
pub struct ParseProgress {
    pub consumed: usize,
    pub record: Option<Record>,
}

#[derive(Clone, Copy)]
struct ParsedHeader {
    record_type: RecordType,
    instance_id: [u8; 16],
    epoch: u64,
    sequence: u64,
    payload_len: usize,
}

/// An aligned, bounded incremental protocol parser.
#[derive(Clone, Debug)]
pub struct Parser {
    header: [u8; HEADER_LEN],
    header_count: usize,
    body: Vec<u8>,
    declared_body_len: Option<usize>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {
            header: [0; HEADER_LEN],
            header_count: 0,
            body: Vec::new(),
            declared_body_len: None,
        }
    }

    /// Accepts bytes up to and including one complete record.
    ///
    /// Any bytes after the returned record remain unconsumed with the caller.
    pub fn accept(&mut self, input: &[u8]) -> Result<ParseProgress, ProtocolError> {
        let mut consumed = 0;
        if self.header_count < HEADER_LEN {
            let take = (HEADER_LEN - self.header_count).min(input.len());
            self.header[self.header_count..self.header_count + take]
                .copy_from_slice(&input[..take]);
            self.header_count += take;
            consumed += take;
            if self.header_count < HEADER_LEN {
                return Ok(ParseProgress {
                    consumed,
                    record: None,
                });
            }

            let header = parse_header(&self.header)?;
            self.declared_body_len = Some(header.payload_len);
            if header.payload_len == 0 {
                let record = record_from_parts(header, Vec::new())?;
                self.reset();
                return Ok(ParseProgress {
                    consumed,
                    record: Some(record),
                });
            }
            self.body = Vec::with_capacity(header.payload_len);
        }

        let declared = self
            .declared_body_len
            .ok_or(ProtocolError::InvalidSnapshot(
                "missing declared body length",
            ))?;
        let remaining =
            declared
                .checked_sub(self.body.len())
                .ok_or(ProtocolError::InvalidSnapshot(
                    "body exceeds declared length",
                ))?;
        let available = &input[consumed..];
        let take = remaining.min(available.len());
        self.body.extend_from_slice(&available[..take]);
        consumed += take;
        if self.body.len() != declared {
            return Ok(ParseProgress {
                consumed,
                record: None,
            });
        }

        let header = parse_header(&self.header)?;
        let body = core::mem::take(&mut self.body);
        let record = record_from_parts(header, body)?;
        self.reset();
        Ok(ParseProgress {
            consumed,
            record: Some(record),
        })
    }

    pub fn snapshot(&self) -> ParserSnapshot {
        ParserSnapshot {
            header_bytes: self.header.to_vec(),
            header_count: self.header_count,
            body_bytes: self.body.clone(),
            declared_body_len: self.declared_body_len.map(|len| len as u32),
        }
    }

    /// Validates a parser snapshot without allocating parser storage.
    pub fn validate_snapshot(snapshot: &ParserSnapshot) -> Result<(), ProtocolError> {
        if snapshot.header_bytes.len() != HEADER_LEN {
            return Err(ProtocolError::InvalidSnapshot(
                "header storage must be exactly 44 bytes",
            ));
        }
        if snapshot.header_count > HEADER_LEN {
            return Err(ProtocolError::InvalidSnapshot(
                "header count exceeds header length",
            ));
        }
        if snapshot.header_count < HEADER_LEN {
            if snapshot.declared_body_len.is_some() || !snapshot.body_bytes.is_empty() {
                return Err(ProtocolError::InvalidSnapshot(
                    "partial header has body state",
                ));
            }
        } else {
            let mut header = [0; HEADER_LEN];
            header.copy_from_slice(&snapshot.header_bytes);
            let parsed = parse_header(&header)?;
            let declared = snapshot
                .declared_body_len
                .ok_or(ProtocolError::InvalidSnapshot(
                    "complete header has no declared body length",
                ))?;
            if declared as usize != parsed.payload_len {
                return Err(ProtocolError::InvalidSnapshot(
                    "declared body length disagrees with header",
                ));
            }
            if parsed.payload_len == 0 || snapshot.body_bytes.len() >= parsed.payload_len {
                return Err(ProtocolError::InvalidSnapshot(
                    "complete record cannot remain in parser state",
                ));
            }
        }
        Ok(())
    }

    /// Restores parser state after validating all bounds before allocation.
    pub fn restore(snapshot: ParserSnapshot) -> Result<Self, ProtocolError> {
        Self::validate_snapshot(&snapshot)?;

        let mut header = [0; HEADER_LEN];
        header.copy_from_slice(&snapshot.header_bytes);
        let body_capacity = snapshot.declared_body_len.unwrap_or(0) as usize;
        let mut body = Vec::with_capacity(body_capacity);
        body.extend_from_slice(&snapshot.body_bytes);
        Ok(Self {
            header,
            header_count: snapshot.header_count,
            body,
            declared_body_len: snapshot.declared_body_len.map(|len| len as usize),
        })
    }

    pub fn is_aligned(&self) -> bool {
        self.header_count == 0
    }

    fn reset(&mut self) {
        self.header = [0; HEADER_LEN];
        self.header_count = 0;
        self.body.clear();
        self.declared_body_len = None;
    }
}

/// Encodes a validated record using the frozen 44-byte header.
pub fn encode(record: &Record) -> Result<Vec<u8>, ProtocolError> {
    validate_record(record)?;
    let payload_len =
        u32::try_from(record.payload.len()).map_err(|_| ProtocolError::LengthOverflow)?;
    let total_len = HEADER_LEN
        .checked_add(record.payload.len())
        .ok_or(ProtocolError::LengthOverflow)?;
    let mut bytes = Vec::with_capacity(total_len);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.push(record.record_type as u8);
    bytes.push(0);
    bytes.extend_from_slice(&record.instance_id);
    bytes.extend_from_slice(&record.epoch.to_le_bytes());
    bytes.extend_from_slice(&record.sequence.to_le_bytes());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    bytes.extend_from_slice(&record.payload);
    Ok(bytes)
}

/// Decodes exactly one complete record.
pub fn decode_exact(bytes: &[u8]) -> Result<Record, ProtocolError> {
    let mut parser = Parser::new();
    let progress = parser.accept(bytes)?;
    if progress.consumed != bytes.len() || !parser.is_aligned() {
        return Err(ProtocolError::InvalidSnapshot(
            "encoded record is truncated or contains trailing bytes",
        ));
    }
    progress.record.ok_or(ProtocolError::InvalidSnapshot(
        "encoded record is incomplete",
    ))
}

fn parse_header(bytes: &[u8; HEADER_LEN]) -> Result<ParsedHeader, ProtocolError> {
    if bytes[..4] != MAGIC {
        return Err(ProtocolError::InvalidMagic);
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        return Err(ProtocolError::InvalidVersion(version));
    }
    let record_type = RecordType::try_from(bytes[6])?;
    if bytes[7] != 0 {
        return Err(ProtocolError::InvalidFlags(bytes[7]));
    }
    let mut instance_id = [0; 16];
    instance_id.copy_from_slice(&bytes[8..24]);
    let epoch = u64::from_le_bytes(bytes[24..32].try_into().map_err(|_| {
        ProtocolError::InvalidSnapshot("epoch field has an invalid encoded length")
    })?);
    let sequence = u64::from_le_bytes(bytes[32..40].try_into().map_err(|_| {
        ProtocolError::InvalidSnapshot("sequence field has an invalid encoded length")
    })?);
    let payload_len = u32::from_le_bytes(bytes[40..44].try_into().map_err(|_| {
        ProtocolError::InvalidSnapshot("payload field has an invalid encoded length")
    })?);
    validate_payload_length(record_type, payload_len)?;
    if matches!(
        record_type,
        RecordType::GuestAttach | RecordType::HostAttach
    ) && (instance_id != [0; 16] || epoch != 0 || sequence != 0)
    {
        return Err(ProtocolError::InvalidBootstrapIdentity);
    }
    Ok(ParsedHeader {
        record_type,
        instance_id,
        epoch,
        sequence,
        payload_len: payload_len as usize,
    })
}

fn record_from_parts(header: ParsedHeader, payload: Vec<u8>) -> Result<Record, ProtocolError> {
    let record = Record {
        record_type: header.record_type,
        instance_id: header.instance_id,
        epoch: header.epoch,
        sequence: header.sequence,
        payload,
    };
    validate_record(&record)?;
    Ok(record)
}

fn validate_record(record: &Record) -> Result<(), ProtocolError> {
    let payload_len =
        u32::try_from(record.payload.len()).map_err(|_| ProtocolError::LengthOverflow)?;
    validate_payload_length(record.record_type, payload_len)?;
    if matches!(
        record.record_type,
        RecordType::GuestAttach | RecordType::HostAttach
    ) && (record.instance_id != [0; 16] || record.epoch != 0 || record.sequence != 0)
    {
        return Err(ProtocolError::InvalidBootstrapIdentity);
    }
    if record.record_type == RecordType::Error {
        let code_bytes: [u8; 4] = record.payload.as_slice().try_into().map_err(|_| {
            ProtocolError::InvalidPayloadLength {
                record_type: RecordType::Error,
                length: payload_len,
            }
        })?;
        let code = u32::from_le_bytes(code_bytes);
        if !(1..=5).contains(&code) {
            return Err(ProtocolError::InvalidErrorCode(code));
        }
    }
    Ok(())
}

fn validate_payload_length(record_type: RecordType, payload_len: u32) -> Result<(), ProtocolError> {
    let valid = match record_type {
        RecordType::GuestAttach
        | RecordType::Reset
        | RecordType::Ack
        | RecordType::Wait
        | RecordType::Ready => payload_len == 0,
        RecordType::HostAttach => payload_len == 32,
        RecordType::Data => (1..=MAX_DATA_LEN as u32).contains(&payload_len),
        RecordType::Error => payload_len == 4,
    };
    if valid {
        Ok(())
    } else {
        Err(ProtocolError::InvalidPayloadLength {
            record_type,
            length: payload_len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn sample(record_type: RecordType, payload: Vec<u8>) -> Record {
        if matches!(
            record_type,
            RecordType::GuestAttach | RecordType::HostAttach
        ) {
            Record::bootstrap(record_type, payload)
        } else {
            Record::session(record_type, [0x11; 16], 7, 9, payload)
        }
    }

    fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
        if !hex.len().is_multiple_of(2) {
            return Err("odd hex length".into());
        }
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = core::str::from_utf8(pair).map_err(|error| error.to_string())?;
                u8::from_str_radix(text, 16).map_err(|error| error.to_string())
            })
            .collect()
    }

    #[test]
    fn golden_vectors_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let vectors = include_str!("../test_data/control_session_protocol_v1_vectors.txt");
        let mut count = 0;
        for line in vectors.lines().filter(|line| !line.is_empty()) {
            let (name, hex) = line
                .split_once('|')
                .ok_or_else(|| format!("invalid vector line: {line}"))?;
            let bytes = decode_hex(hex)?;
            let record = decode_exact(&bytes)?;
            let expected_type = match name {
                "GUEST_ATTACH" => RecordType::GuestAttach,
                "HOST_ATTACH" => RecordType::HostAttach,
                "RESET" => RecordType::Reset,
                "ACK" => RecordType::Ack,
                "DATA_EMBEDDED_MAGIC" => RecordType::Data,
                "WAIT" => RecordType::Wait,
                "READY" => RecordType::Ready,
                "ERROR_AUTHENTICATION" => RecordType::Error,
                _ => return Err(format!("unknown vector name: {name}").into()),
            };
            assert_eq!(record.record_type, expected_type, "{name}");
            if matches!(
                expected_type,
                RecordType::GuestAttach | RecordType::HostAttach
            ) {
                assert_eq!(record.instance_id, [0; 16], "{name}");
                assert_eq!((record.epoch, record.sequence), (0, 0), "{name}");
            } else {
                assert_eq!(
                    record.instance_id,
                    core::array::from_fn(|index| index as u8),
                    "{name}"
                );
                assert_eq!((record.epoch, record.sequence), (7, 9), "{name}");
            }
            if expected_type == RecordType::HostAttach {
                assert_eq!(
                    record.payload,
                    (0..32).map(|value| value as u8).collect::<Vec<_>>()
                );
            }
            if expected_type == RecordType::Data {
                assert_eq!(record.payload, b"NVXSNVXS");
            }
            assert_eq!(encode(&record)?, bytes, "{name}");
            count += 1;
        }
        assert_eq!(count, 8);
        assert!(vectors.contains("4e5658534e565853"));
        Ok(())
    }

    #[test]
    fn language_neutral_boundary_and_invalid_cases() -> Result<(), Box<dyn std::error::Error>> {
        let cases = include_str!("../test_data/control_session_protocol_v1_cases.txt");
        let instance_id = core::array::from_fn(|index| index as u8);
        let mut count = 0;
        for line in cases.lines().filter(|line| !line.is_empty()) {
            let fields = line.split('|').collect::<Vec<_>>();
            match fields.as_slice() {
                ["DATA_LENGTH", name, length] => {
                    let length = length.parse::<usize>()?;
                    let result = encode(&Record::session(
                        RecordType::Data,
                        instance_id,
                        7,
                        9,
                        vec![0x5a; length],
                    ));
                    assert_eq!(result.is_ok(), name.starts_with("VALID_"), "{name}");
                }
                ["INVALID_HEADER", name, hex] => {
                    let bytes = decode_hex(hex)?;
                    assert_eq!(bytes.len(), HEADER_LEN, "{name}");
                    let mut parser = Parser::new();
                    assert!(parser.accept(&bytes).is_err(), "{name}");
                }
                _ => return Err(format!("invalid language-neutral case: {line}").into()),
            }
            count += 1;
        }
        assert_eq!(count, 9);
        Ok(())
    }

    #[test]
    fn every_byte_fragmentation_preserves_state() -> Result<(), ProtocolError> {
        for payload in [vec![0x55], vec![0x33; 257]] {
            let record = sample(RecordType::Data, payload);
            let encoded = encode(&record)?;
            for split in 0..encoded.len() {
                let mut parser = Parser::new();
                let first = parser.accept(&encoded[..split])?;
                assert!(first.record.is_none());
                let restored = Parser::restore(parser.snapshot())?;
                parser = restored;
                let second = parser.accept(&encoded[split..])?;
                assert_eq!(second.record, Some(record.clone()));
            }
        }
        Ok(())
    }

    #[test]
    fn coalesced_records_stop_at_exact_boundary() -> Result<(), ProtocolError> {
        let first = sample(RecordType::Ready, Vec::new());
        let second = sample(RecordType::Data, b"xNVXSy".to_vec());
        let mut bytes = encode(&first)?;
        bytes.extend_from_slice(&encode(&second)?);
        let mut parser = Parser::new();
        let progress = parser.accept(&bytes)?;
        assert_eq!(progress.record, Some(first));
        assert_eq!(progress.consumed, HEADER_LEN);
        let progress = parser.accept(&bytes[progress.consumed..])?;
        assert_eq!(progress.record, Some(second));
        Ok(())
    }

    #[test]
    fn payload_boundaries() -> Result<(), ProtocolError> {
        assert!(encode(&sample(RecordType::Data, vec![1])).is_ok());
        assert!(encode(&sample(RecordType::Data, vec![1; MAX_DATA_LEN])).is_ok());
        assert!(matches!(
            encode(&sample(RecordType::Data, Vec::new())),
            Err(ProtocolError::InvalidPayloadLength { length: 0, .. })
        ));
        assert!(matches!(
            encode(&sample(RecordType::Data, vec![1; MAX_DATA_LEN + 1])),
            Err(ProtocolError::InvalidPayloadLength { length: 65_537, .. })
        ));
        Ok(())
    }

    #[test]
    fn malformed_headers_are_rejected_before_body_allocation() -> Result<(), ProtocolError> {
        let valid = encode(&sample(RecordType::Data, vec![1]))?;
        for (offset, value) in [(0, b'X'), (4, 2), (6, 99), (7, 1)] {
            let mut malformed = valid.clone();
            malformed[offset] = value;
            let mut parser = Parser::new();
            assert!(parser.accept(&malformed[..HEADER_LEN]).is_err());
            assert!(parser.body.capacity() <= MAX_DATA_LEN);
        }
        let mut oversized = valid;
        oversized[40..44].copy_from_slice(&65_537u32.to_le_bytes());
        let mut parser = Parser::new();
        assert!(matches!(
            parser.accept(&oversized[..HEADER_LEN]),
            Err(ProtocolError::InvalidPayloadLength { length: 65_537, .. })
        ));
        assert_eq!(parser.body.capacity(), 0);
        Ok(())
    }

    #[test]
    fn malformed_magic_is_not_resynchronized() -> Result<(), ProtocolError> {
        let valid = encode(&sample(RecordType::Data, b"NVXS".to_vec()))?;
        let mut malformed = vec![0; HEADER_LEN];
        malformed.extend_from_slice(&valid);
        let mut parser = Parser::new();
        assert_eq!(parser.accept(&malformed), Err(ProtocolError::InvalidMagic));
        Ok(())
    }

    #[test]
    fn payload_rules_and_error_codes_are_enforced() {
        assert!(encode(&sample(RecordType::GuestAttach, vec![1])).is_err());
        assert!(encode(&sample(RecordType::HostAttach, vec![0; 31])).is_err());
        assert!(encode(&sample(RecordType::Reset, vec![1])).is_err());
        assert!(encode(&sample(RecordType::Ack, vec![1])).is_err());
        assert!(encode(&sample(RecordType::Wait, vec![1])).is_err());
        assert!(encode(&sample(RecordType::Ready, vec![1])).is_err());
        assert!(encode(&sample(RecordType::Error, 0u32.to_le_bytes().to_vec())).is_err());
        assert!(encode(&sample(RecordType::Error, 6u32.to_le_bytes().to_vec())).is_err());
    }

    #[test]
    fn invalid_parser_snapshots_are_rejected() {
        let mut snapshot = Parser::new().snapshot();
        snapshot.header_count = HEADER_LEN + 1;
        assert!(Parser::restore(snapshot).is_err());

        let mut snapshot = Parser::new().snapshot();
        snapshot.body_bytes.push(1);
        assert!(Parser::restore(snapshot).is_err());

        let mut snapshot = Parser::new().snapshot();
        snapshot.header_bytes.pop();
        assert!(Parser::restore(snapshot).is_err());
    }
}
