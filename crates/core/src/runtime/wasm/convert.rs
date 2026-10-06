//! Variant ↔ EVE/1.

use n2link_eve::{EveValue, Limits, decode, encode};

use crate::N2linkError;
use crate::runtime::model::Variant;

pub(crate) fn variant_to_eve(value: &Variant) -> crate::Result<EveValue> {
    Ok(match value {
        Variant::Null => EveValue::Null,
        Variant::Bool(flag) => EveValue::Bool(*flag),
        Variant::Number(number) => {
            if let Some(n) = number.as_i64() {
                EveValue::I64(n)
            } else if let Some(n) = number.as_u64() {
                EveValue::U64(n)
            } else if let Some(n) = number.as_f64() {
                EveValue::F64(n)
            } else {
                return Err(N2linkError::invalid_operation("number has no EVE form"));
            }
        }
        Variant::String(text) => EveValue::String(text.clone()),
        Variant::Bytes(bytes) => EveValue::Bytes(bytes.clone()),
        Variant::Array(items) => EveValue::Array(items.iter().map(variant_to_eve).collect::<crate::Result<Vec<_>>>()?),
        Variant::Object(map) => {
            let mut pairs = Vec::new();
            for (key, item) in map.iter() {
                pairs.push((key.clone(), variant_to_eve(item)?));
            }
            EveValue::Object(pairs)
        }
        Variant::Date(time) => EveValue::Date(date_to_ms(*time)?),
        Variant::Regexp(re) => EveValue::Regexp(re.as_str().to_owned()),
    })
}

pub(crate) fn eve_to_variant(value: &EveValue) -> crate::Result<Variant> {
    Ok(match value {
        EveValue::Null => Variant::Null,
        EveValue::Bool(flag) => Variant::Bool(*flag),
        EveValue::I64(n) => Variant::from(*n),
        EveValue::U64(n) => Variant::from(*n),
        EveValue::F64(n) => serde_json::Number::from_f64(*n)
            .map(Variant::Number)
            .ok_or_else(|| N2linkError::invalid_operation("WASM output: non-finite number"))?,
        EveValue::String(text) => Variant::String(text.clone()),
        EveValue::Bytes(bytes) => Variant::Bytes(bytes.clone()),
        EveValue::Array(items) => Variant::Array(items.iter().map(eve_to_variant).collect::<crate::Result<Vec<_>>>()?),
        EveValue::Object(pairs) => {
            let mut map = crate::runtime::model::VariantObjectMap::new();
            for (key, item) in pairs {
                map.insert(key.clone(), eve_to_variant(item)?);
            }
            Variant::Object(map)
        }
        EveValue::Date(ms) => Variant::Date(ms_to_date(*ms)?),
        EveValue::Regexp(source) => {
            // Guest-supplied pattern: bound the compiled program size.
            let re = regex::RegexBuilder::new(source)
                .size_limit(REGEX_SIZE_LIMIT)
                .dfa_size_limit(REGEX_SIZE_LIMIT)
                .build()
                .map_err(|err| N2linkError::invalid_operation(&format!("WASM output regexp: {err}")))?;
            Variant::Regexp(re)
        }
    })
}

const REGEX_SIZE_LIMIT: usize = 1024 * 1024;

fn date_to_ms(time: std::time::SystemTime) -> crate::Result<i64> {
    let out_of_range = || N2linkError::invalid_operation("date is out of the EVE/1 range");
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).map_err(|_| out_of_range()),
        Err(before) => i64::try_from(before.duration().as_millis()).map(|ms| -ms).map_err(|_| out_of_range()),
    }
}

fn ms_to_date(ms: i64) -> crate::Result<std::time::SystemTime> {
    let offset = std::time::Duration::from_millis(ms.unsigned_abs());
    let date =
        if ms >= 0 { std::time::UNIX_EPOCH.checked_add(offset) } else { std::time::UNIX_EPOCH.checked_sub(offset) };
    date.ok_or_else(|| N2linkError::invalid_operation("WASM output: date is not representable"))
}

pub(crate) fn encode_variant(value: &Variant) -> crate::Result<Vec<u8>> {
    encode(&variant_to_eve(value)?).map_err(|err| N2linkError::invalid_operation(&err.to_string()))
}

pub(crate) fn decode_variant(bytes: &[u8], max_len: u32) -> crate::Result<Variant> {
    let value = decode(bytes, Limits { max_depth: 32, max_values: 65_536, max_len })
        .map_err(|err| N2linkError::invalid_operation(&err.to_string()))?;
    eve_to_variant(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn dates_before_the_epoch_round_trip() {
        let date = UNIX_EPOCH - Duration::from_millis(86_400_123);
        let bytes = encode_variant(&Variant::Date(date)).unwrap();
        match decode_variant(&bytes, 1024).unwrap() {
            Variant::Date(back) => assert_eq!(back, date),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn buffers_keep_their_type() {
        let bytes = encode_variant(&Variant::Bytes(vec![0, 255])).unwrap();
        assert!(matches!(decode_variant(&bytes, 1024).unwrap(), Variant::Bytes(b) if b == vec![0, 255]));
    }
}
