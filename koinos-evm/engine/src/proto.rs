//! Minimal protobuf wire format encoder/decoder for no_std.
//!
//! Only supports the subset needed for Koinos system calls:
//! - Varint (wire type 0): bool, uint32, uint64, int32, enum
//! - Length-delimited (wire type 2): bytes, string, submessages

use alloc::vec::Vec;

// Wire types
pub const WIRE_VARINT: u8 = 0;
pub const WIRE_LENGTH_DELIMITED: u8 = 2;

/// Encode a varint into the buffer.
pub fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// Decode a varint from a byte slice, returning (value, bytes_consumed).
/// Enforces a 10-byte maximum and rejects overflowed encodings.
pub fn decode_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &byte) in data.iter().enumerate() {
        if i >= 10 {
            return None; // varints cannot exceed 10 bytes
        }
        let payload = (byte & 0x7F) as u64;
        // On the 10th byte (shift=63), only bit 0 is valid
        if shift == 63 && payload > 1 {
            return None;
        }
        result |= payload << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
    }
    None
}

/// Encode a field tag (field_number, wire_type).
pub fn encode_tag(buf: &mut Vec<u8>, field_number: u32, wire_type: u8) {
    encode_varint(buf, ((field_number as u64) << 3) | (wire_type as u64));
}

/// Encode a varint field.
pub fn encode_varint_field(buf: &mut Vec<u8>, field_number: u32, value: u64) {
    if value == 0 {
        return; // protobuf default, skip zero values
    }
    encode_tag(buf, field_number, WIRE_VARINT);
    encode_varint(buf, value);
}

/// Encode a bool field.
pub fn encode_bool_field(buf: &mut Vec<u8>, field_number: u32, value: bool) {
    if !value {
        return;
    }
    encode_tag(buf, field_number, WIRE_VARINT);
    encode_varint(buf, 1);
}

/// Encode a bytes/string field.
pub fn encode_bytes_field(buf: &mut Vec<u8>, field_number: u32, value: &[u8]) {
    if value.is_empty() {
        return;
    }
    encode_tag(buf, field_number, WIRE_LENGTH_DELIMITED);
    encode_varint(buf, value.len() as u64);
    buf.extend_from_slice(value);
}

/// Encode a submessage field.
pub fn encode_submessage_field(buf: &mut Vec<u8>, field_number: u32, submessage: &[u8]) {
    if submessage.is_empty() {
        return;
    }
    encode_tag(buf, field_number, WIRE_LENGTH_DELIMITED);
    encode_varint(buf, submessage.len() as u64);
    buf.extend_from_slice(submessage);
}

/// Encode a sint32 using ZigZag encoding.
pub fn encode_sint32(value: i32) -> u64 {
    ((value << 1) ^ (value >> 31)) as u32 as u64
}

/// A simple protobuf field decoder/iterator.
pub struct FieldIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> FieldIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

/// Decoded protobuf field.
pub enum FieldValue<'a> {
    Varint(u64),
    LengthDelimited(&'a [u8]),
}

/// (field_number, field_value)
pub type Field<'a> = (u32, FieldValue<'a>);

impl<'a> Iterator for FieldIter<'a> {
    type Item = Field<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        let (tag, consumed) = decode_varint(&self.data[self.pos..])?;
        self.pos += consumed;

        let field_number_u64 = tag >> 3;
        if field_number_u64 == 0 || field_number_u64 > u32::MAX as u64 {
            return None; // invalid field number
        }
        let field_number = field_number_u64 as u32;
        let wire_type = (tag & 0x07) as u8;

        match wire_type {
            WIRE_VARINT => {
                let (value, consumed) = decode_varint(&self.data[self.pos..])?;
                self.pos += consumed;
                Some((field_number, FieldValue::Varint(value)))
            }
            WIRE_LENGTH_DELIMITED => {
                let (len, consumed) = decode_varint(&self.data[self.pos..])?;
                self.pos += consumed;
                let len = usize::try_from(len).ok()?;
                let end = self.pos.checked_add(len)?;
                if end > self.data.len() {
                    return None;
                }
                let value = &self.data[self.pos..end];
                self.pos += len;
                Some((field_number, FieldValue::LengthDelimited(value)))
            }
            _ => {
                // Skip unknown wire types
                None
            }
        }
    }
}

/// Helper: extract a varint field value.
pub fn get_varint(field: &FieldValue) -> Option<u64> {
    match field {
        FieldValue::Varint(v) => Some(*v),
        _ => None,
    }
}

/// Helper: extract a bytes field value.
pub fn get_bytes<'a>(field: &'a FieldValue<'a>) -> Option<&'a [u8]> {
    match field {
        FieldValue::LengthDelimited(v) => Some(v),
        _ => None,
    }
}
