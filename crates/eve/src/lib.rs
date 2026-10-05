//! EVE/1 encoding: `0xEE 0x01` then one value. Little-endian. `no_std` + `alloc`.
#![no_std]
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

const MAGIC: u8 = 0xEE;
const VERSION: u8 = 0x01;

const TAG_NULL: u8 = 0x00;
const TAG_FALSE: u8 = 0x01;
const TAG_TRUE: u8 = 0x02;
const TAG_I64: u8 = 0x03;
const TAG_U64: u8 = 0x04;
const TAG_F64: u8 = 0x05;
const TAG_STRING: u8 = 0x06;
const TAG_BYTES: u8 = 0x07;
const TAG_ARRAY: u8 = 0x08;
const TAG_OBJECT: u8 = 0x09;
const TAG_DATE: u8 = 0x0A;
const TAG_REGEXP: u8 = 0x0B;

#[derive(Debug, Clone, PartialEq)]
pub enum EveValue {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<EveValue>),
    Object(Vec<(String, EveValue)>),
    Date(i64),
    Regexp(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    pub offset: usize,
    pub message: &'static str,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "eve decode at {}: {}", self.offset, self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeError {
    pub message: &'static str,
}

impl core::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "eve encode: {}", self.message)
    }
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub max_depth: u32,
    pub max_values: u32,
    pub max_len: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_depth: 32, max_values: 65_536, max_len: 1024 * 1024 }
    }
}

pub fn encode(value: &EveValue) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    out.push(MAGIC);
    out.push(VERSION);
    encode_value(value, &mut out, 0)?;
    Ok(out)
}

fn encode_value(value: &EveValue, out: &mut Vec<u8>, depth: u32) -> Result<(), EncodeError> {
    if depth > 32 {
        return Err(EncodeError { message: "depth exceeds 32" });
    }
    match value {
        EveValue::Null => out.push(TAG_NULL),
        EveValue::Bool(false) => out.push(TAG_FALSE),
        EveValue::Bool(true) => out.push(TAG_TRUE),
        EveValue::I64(n) => {
            out.push(TAG_I64);
            out.extend_from_slice(&n.to_le_bytes());
        }
        EveValue::U64(n) => {
            out.push(TAG_U64);
            out.extend_from_slice(&n.to_le_bytes());
        }
        EveValue::F64(n) => {
            if !n.is_finite() {
                return Err(EncodeError { message: "NaN and infinities are not allowed" });
            }
            out.push(TAG_F64);
            out.extend_from_slice(&n.to_le_bytes());
        }
        EveValue::String(text) => {
            out.push(TAG_STRING);
            write_bytes(out, text.as_bytes())?;
        }
        EveValue::Bytes(bytes) => {
            out.push(TAG_BYTES);
            write_bytes(out, bytes)?;
        }
        EveValue::Array(items) => {
            out.push(TAG_ARRAY);
            write_u32(out, items.len() as u32);
            for item in items {
                encode_value(item, out, depth + 1)?;
            }
        }
        EveValue::Object(pairs) => {
            out.push(TAG_OBJECT);
            write_u32(out, pairs.len() as u32);
            for (key, item) in pairs {
                write_bytes(out, key.as_bytes())?;
                encode_value(item, out, depth + 1)?;
            }
        }
        EveValue::Date(ms) => {
            out.push(TAG_DATE);
            out.extend_from_slice(&ms.to_le_bytes());
        }
        EveValue::Regexp(source) => {
            out.push(TAG_REGEXP);
            write_bytes(out, source.as_bytes())?;
        }
    }
    Ok(())
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), EncodeError> {
    if bytes.len() > u32::MAX as usize {
        return Err(EncodeError { message: "length exceeds u32" });
    }
    write_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
    Ok(())
}

pub fn decode(bytes: &[u8], limits: Limits) -> Result<EveValue, DecodeError> {
    if bytes.len() < 2 || bytes[0] != MAGIC || bytes[1] != VERSION {
        return Err(DecodeError { offset: 0, message: "bad magic or version" });
    }
    let mut cur = Cursor { input: bytes, offset: 2, values: 0, limits };
    let value = cur.read_value(0)?;
    if cur.offset != bytes.len() {
        return Err(DecodeError { offset: cur.offset, message: "trailing bytes" });
    }
    Ok(value)
}

struct Cursor<'a> {
    input: &'a [u8],
    offset: usize,
    values: u32,
    limits: Limits,
}

impl<'a> Cursor<'a> {
    fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.offset)
    }

    fn need(&self, n: usize) -> Result<(), DecodeError> {
        if self.remaining() < n {
            Err(DecodeError { offset: self.offset, message: "truncated" })
        } else {
            Ok(())
        }
    }

    fn read_u8(&mut self) -> Result<u8, DecodeError> {
        self.need(1)?;
        let b = self.input[self.offset];
        self.offset += 1;
        Ok(b)
    }

    fn read_exact(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        self.need(n)?;
        let slice = &self.input[self.offset..self.offset + n];
        self.offset += n;
        Ok(slice)
    }

    fn read_u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_i64(&mut self) -> Result<i64, DecodeError> {
        let bytes = self.read_exact(8)?;
        Ok(i64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_len(&mut self) -> Result<usize, DecodeError> {
        let len = self.read_u32()? as usize;
        if len as u32 > self.limits.max_len {
            return Err(DecodeError { offset: self.offset, message: "length exceeds max_len" });
        }
        if len > self.remaining() {
            return Err(DecodeError { offset: self.offset, message: "count exceeds remaining" });
        }
        Ok(len)
    }

    fn count_value(&mut self) -> Result<(), DecodeError> {
        self.values = self.values.saturating_add(1);
        if self.values > self.limits.max_values {
            return Err(DecodeError { offset: self.offset, message: "too many values" });
        }
        Ok(())
    }

    fn read_value(&mut self, depth: u32) -> Result<EveValue, DecodeError> {
        if depth > self.limits.max_depth {
            return Err(DecodeError { offset: self.offset, message: "depth exceeds max_depth" });
        }
        self.count_value()?;
        match self.read_u8()? {
            TAG_NULL => Ok(EveValue::Null),
            TAG_FALSE => Ok(EveValue::Bool(false)),
            TAG_TRUE => Ok(EveValue::Bool(true)),
            TAG_I64 => Ok(EveValue::I64(self.read_i64()?)),
            TAG_U64 => Ok(EveValue::U64(self.read_u64()?)),
            TAG_F64 => {
                let bits = self.read_u64()?;
                let n = f64::from_bits(bits);
                if !n.is_finite() {
                    return Err(DecodeError { offset: self.offset, message: "NaN and infinities are not allowed" });
                }
                Ok(EveValue::F64(n))
            }
            TAG_STRING => {
                let len = self.read_len()?;
                let bytes = self.read_exact(len)?;
                let text = core::str::from_utf8(bytes)
                    .map_err(|_| DecodeError { offset: self.offset, message: "invalid UTF-8" })?;
                Ok(EveValue::String(text.into()))
            }
            TAG_BYTES => {
                let len = self.read_len()?;
                let bytes = self.read_exact(len)?;
                Ok(EveValue::Bytes(bytes.to_vec()))
            }
            TAG_ARRAY => {
                let count = self.read_u32()? as usize;
                if count > self.remaining() {
                    return Err(DecodeError { offset: self.offset, message: "count exceeds remaining" });
                }
                let mut items = Vec::new();
                for _ in 0..count {
                    items.push(self.read_value(depth + 1)?);
                }
                Ok(EveValue::Array(items))
            }
            TAG_OBJECT => {
                let count = self.read_u32()? as usize;
                if count > self.remaining() {
                    return Err(DecodeError { offset: self.offset, message: "count exceeds remaining" });
                }
                let mut pairs = Vec::new();
                for _ in 0..count {
                    let key_len = self.read_len()?;
                    let key_bytes = self.read_exact(key_len)?;
                    let key = core::str::from_utf8(key_bytes)
                        .map_err(|_| DecodeError { offset: self.offset, message: "invalid UTF-8" })?;
                    let key = String::from(key);
                    if pairs.iter().any(|(existing, _)| existing == &key) {
                        return Err(DecodeError { offset: self.offset, message: "duplicate key" });
                    }
                    let value = self.read_value(depth + 1)?;
                    pairs.push((key, value));
                }
                Ok(EveValue::Object(pairs))
            }
            TAG_DATE => Ok(EveValue::Date(self.read_i64()?)),
            TAG_REGEXP => {
                let len = self.read_len()?;
                let bytes = self.read_exact(len)?;
                let text = core::str::from_utf8(bytes)
                    .map_err(|_| DecodeError { offset: self.offset, message: "invalid UTF-8" })?;
                Ok(EveValue::Regexp(text.into()))
            }
            _ => Err(DecodeError { offset: self.offset.saturating_sub(1), message: "unknown tag" }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use alloc::vec;

    fn round(value: EveValue) -> EveValue {
        decode(&encode(&value).unwrap(), Limits::default()).unwrap()
    }

    #[test]
    fn round_trip_scalars() {
        assert_eq!(round(EveValue::Null), EveValue::Null);
        assert_eq!(round(EveValue::Bool(true)), EveValue::Bool(true));
        assert_eq!(round(EveValue::I64(-3)), EveValue::I64(-3));
        assert_eq!(round(EveValue::U64(u64::MAX)), EveValue::U64(u64::MAX));
        assert_eq!(round(EveValue::F64(1.5)), EveValue::F64(1.5));
        assert_eq!(round(EveValue::String("hi".into())), EveValue::String("hi".into()));
        assert_eq!(round(EveValue::Bytes(vec![1, 2])), EveValue::Bytes(vec![1, 2]));
        assert_eq!(round(EveValue::Date(0)), EveValue::Date(0));
        assert_eq!(round(EveValue::Regexp("a+".into())), EveValue::Regexp("a+".into()));
    }

    #[test]
    fn nan_is_rejected() {
        assert!(encode(&EveValue::F64(f64::NAN)).is_err());
    }

    #[test]
    fn trailing_bytes_fail() {
        let mut bytes = encode(&EveValue::Null).unwrap();
        bytes.push(0);
        assert!(decode(&bytes, Limits::default()).is_err());
    }

    #[test]
    fn unknown_tag_fails() {
        let bytes = [MAGIC, VERSION, 0x7F];
        assert!(decode(&bytes, Limits::default()).is_err());
    }
}
