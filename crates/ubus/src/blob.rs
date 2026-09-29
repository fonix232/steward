//! libubox's blob and blobmsg encodings, as ubus carries them.
//!
//! A blob attribute is a big-endian `u32` header, then its payload, padded to
//! four bytes. The header holds the attribute's length (header included,
//! padding not) in its low 24 bits, an id in the next 7, and the "extended"
//! flag in the top bit. A blobmsg attribute is an extended one whose payload
//! starts with a name (big-endian `u16` length, the name, a NUL, padded to four
//! bytes) and whose id is its type. Tables and arrays nest attributes.

use serde_json::{Map, Number, Value};
use std::fmt;

const EXTENDED: u32 = 0x8000_0000;
const ID_MASK: u32 = 0x7f00_0000;
const ID_SHIFT: u32 = 24;
const LEN_MASK: u32 = 0x00ff_ffff;

// blobmsg types (libubox blobmsg.h). BOOL is INT8.
const UNSPEC: u8 = 0;
const ARRAY: u8 = 1;
const TABLE: u8 = 2;
const STRING: u8 = 3;
const INT64: u8 = 4;
const INT32: u8 = 5;
const INT16: u8 = 6;
const INT8: u8 = 7;
const DOUBLE: u8 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed blob: {}", self.0)
    }
}

impl std::error::Error for Malformed {}

pub(crate) fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// One attribute of a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr<'a> {
    pub id: u8,
    pub extended: bool,
    pub payload: &'a [u8],
}

/// Appends an attribute and its padding.
pub fn put(buf: &mut Vec<u8>, id: u8, extended: bool, payload: &[u8]) {
    let len = 4 + payload.len();
    assert!(len as u32 <= LEN_MASK, "blob attribute too long");
    let hdr =
        if extended { EXTENDED } else { 0 } | (u32::from(id) << ID_SHIFT) & ID_MASK | len as u32;
    buf.extend_from_slice(&hdr.to_be_bytes());
    buf.extend_from_slice(payload);
    buf.resize(buf.len() + pad4(len) - len, 0);
}

/// Reads the attribute at the start of `data`: it and the rest after its
/// padding.
pub fn take(data: &[u8]) -> Result<(Attr<'_>, &[u8]), Malformed> {
    let hdr = data
        .get(..4)
        .ok_or(Malformed("attribute header cut short"))?;
    let hdr = u32::from_be_bytes(hdr.try_into().unwrap());
    let len = (hdr & LEN_MASK) as usize;
    if len < 4 || len > data.len() {
        return Err(Malformed("attribute length out of range"));
    }
    let attr = Attr {
        id: ((hdr & ID_MASK) >> ID_SHIFT) as u8,
        extended: hdr & EXTENDED != 0,
        payload: &data[4..len],
    };
    Ok((attr, &data[pad4(len).min(data.len())..]))
}

/// Every attribute in `data`, in order.
pub fn attrs(mut data: &[u8]) -> Result<Vec<Attr<'_>>, Malformed> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let (a, rest) = take(data)?;
        out.push(a);
        data = rest;
    }
    Ok(out)
}

/// Appends `value` as a blobmsg attribute called `name` (empty inside arrays),
/// typed the way libubox's blobmsg_add_json_element types JSON: booleans as
/// INT8, integers as INT32 when they fit and INT64 otherwise, other numbers
/// as DOUBLE, null as UNSPEC.
pub fn put_value(buf: &mut Vec<u8>, name: &str, value: &Value) {
    let mut payload = Vec::new();
    let namelen = u16::try_from(name.len()).expect("blobmsg name too long");
    payload.extend_from_slice(&namelen.to_be_bytes());
    payload.extend_from_slice(name.as_bytes());
    payload.push(0);
    payload.resize(pad4(payload.len()), 0);
    let ty = match value {
        Value::Null => UNSPEC,
        Value::Bool(b) => {
            payload.push(u8::from(*b));
            INT8
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if let Ok(i) = i32::try_from(i) {
                    payload.extend_from_slice(&i.to_be_bytes());
                    INT32
                } else {
                    payload.extend_from_slice(&i.to_be_bytes());
                    INT64
                }
            } else if let Some(u) = n.as_u64() {
                payload.extend_from_slice(&u.to_be_bytes());
                INT64
            } else {
                payload.extend_from_slice(&n.as_f64().unwrap_or(0.0).to_bits().to_be_bytes());
                DOUBLE
            }
        }
        Value::String(s) => {
            payload.extend_from_slice(s.as_bytes());
            payload.push(0);
            STRING
        }
        Value::Array(items) => {
            for v in items {
                put_value(&mut payload, "", v);
            }
            ARRAY
        }
        Value::Object(map) => {
            put_table(&mut payload, map);
            TABLE
        }
    };
    put(buf, ty, true, &payload);
}

/// Appends each member of `map` as a blobmsg attribute: a table's contents.
pub fn put_table(buf: &mut Vec<u8>, map: &Map<String, Value>) {
    for (k, v) in map {
        put_value(buf, k, v);
    }
}

/// A blobmsg attribute's name and value.
pub fn value(attr: Attr<'_>) -> Result<(String, Value), Malformed> {
    let p = attr.payload;
    let (name, data) = if attr.extended {
        let namelen = u16::from_be_bytes(
            p.get(..2)
                .ok_or(Malformed("name cut short"))?
                .try_into()
                .unwrap(),
        ) as usize;
        let hdrlen = pad4(2 + namelen + 1);
        let name = p.get(2..2 + namelen).ok_or(Malformed("name cut short"))?;
        let data = p.get(hdrlen..).ok_or(Malformed("name header cut short"))?;
        (String::from_utf8_lossy(name).into_owned(), data)
    } else {
        (String::new(), p)
    };
    let fixed = |n: usize| data.get(..n).ok_or(Malformed("number cut short"));
    let v = match attr.id {
        UNSPEC => Value::Null,
        INT8 => Value::Bool(fixed(1)?[0] != 0),
        INT16 => Value::from(i16::from_be_bytes(fixed(2)?.try_into().unwrap())),
        INT32 => Value::from(i32::from_be_bytes(fixed(4)?.try_into().unwrap())),
        INT64 => Value::from(i64::from_be_bytes(fixed(8)?.try_into().unwrap())),
        DOUBLE => Number::from_f64(f64::from_bits(u64::from_be_bytes(
            fixed(8)?.try_into().unwrap(),
        )))
        .map_or(Value::Null, Value::Number),
        STRING => {
            let s = data.split(|&b| b == 0).next().unwrap_or_default();
            Value::String(String::from_utf8_lossy(s).into_owned())
        }
        ARRAY => Value::Array(
            attrs(data)?
                .into_iter()
                .map(|a| value(a).map(|(_, v)| v))
                .collect::<Result<_, _>>()?,
        ),
        TABLE => Value::Object(table(data)?),
        _ => return Err(Malformed("unknown blobmsg type")),
    };
    Ok((name, v))
}

/// A table's contents (a list of named blobmsg attributes) as a JSON object.
pub fn table(data: &[u8]) -> Result<Map<String, Value>, Malformed> {
    attrs(data)?.into_iter().map(value).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_string_matches_libubox_byte_for_byte() {
        // blobmsg_add_string(&b, "a", "xy"): extended STRING, name "a",
        // header 4 + name header 4 (2 + 1 + NUL) + "xy\0" = 11, padded to 12.
        let mut buf = Vec::new();
        put_value(&mut buf, "a", &json!("xy"));
        assert_eq!(buf, [0x83, 0, 0, 11, 0, 1, b'a', 0, b'x', b'y', 0, 0]);
    }

    #[test]
    fn values_round_trip() {
        let v = json!({
            "config": "network", "rollback": true, "timeout": 30, "big": 5_000_000_000_i64,
            "neg": -1, "ratio": 0.5, "none": null,
            "ports": ["lan1", "lan2:t"], "values": { "vlan": "10", "nested": [1, [2, 3], {}] },
            "empty": ""
        });
        let mut buf = Vec::new();
        put_table(&mut buf, v.as_object().unwrap());
        assert_eq!(Value::Object(table(&buf).unwrap()), v);
    }

    #[test]
    fn a_truncated_blob_is_refused() {
        let mut buf = Vec::new();
        put_value(&mut buf, "name", &json!("value"));
        for cut in 1..buf.len() - 3 {
            assert!(table(&buf[..cut]).is_err(), "cut at {cut}");
        }
    }
}
