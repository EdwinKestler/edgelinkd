//! Minimal Wasm section walker: header check, section bounds, and the `edgelink.manifest`
//! custom section. Runs before Wasmi sees the bytes, so malformed framing fails with a precise
//! message and no allocation proportional to forged lengths.

use crate::N2linkError;

pub(crate) const MANIFEST_SECTION: &str = "edgelink.manifest";
pub(crate) const MAX_MANIFEST_BYTES: usize = 16 * 1024;

const MAGIC: &[u8; 4] = b"\0asm";
const VERSION: [u8; 4] = [1, 0, 0, 0];

fn invalid(offset: usize, why: &str) -> N2linkError {
    N2linkError::invalid_operation(&format!("invalid WASM package at byte {offset}: {why}"))
}

/// Unsigned LEB128, at most 5 bytes (u32).
fn read_u32(bytes: &[u8], offset: &mut usize) -> crate::Result<u32> {
    let mut result: u32 = 0;
    for i in 0..5 {
        let byte = *bytes.get(*offset).ok_or_else(|| invalid(*offset, "truncated LEB128"))?;
        *offset += 1;
        if i == 4 && byte & 0xF0 != 0 {
            return Err(invalid(*offset - 1, "LEB128 overflows u32"));
        }
        result |= u32::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(invalid(*offset, "LEB128 longer than 5 bytes"))
}

/// The manifest text, after checking the module framing and size cap.
pub(crate) fn manifest_text(bytes: &[u8], max_module_bytes: usize) -> crate::Result<String> {
    if bytes.len() > max_module_bytes {
        return Err(N2linkError::NotSupported(format!(
            "WASM package is {} bytes, more than {max_module_bytes} ([runtime.wasm] max_module_kib)",
            bytes.len()
        )));
    }
    if bytes.len() < 8 || &bytes[0..4] != MAGIC {
        return Err(invalid(0, "not a WebAssembly module"));
    }
    if bytes[4..8] != VERSION {
        return Err(invalid(4, "unsupported WebAssembly binary version"));
    }
    let mut offset = 8;
    let mut manifest: Option<String> = None;
    while offset < bytes.len() {
        let id = bytes[offset];
        offset += 1;
        let size = read_u32(bytes, &mut offset)? as usize;
        let start = offset;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| invalid(start, "section extends past the end of the module"))?;
        if id == 0 {
            let mut cursor = start;
            let name_len = read_u32(bytes, &mut cursor)? as usize;
            let name_end = cursor
                .checked_add(name_len)
                .filter(|e| *e <= end)
                .ok_or_else(|| invalid(cursor, "custom section name"))?;
            let name = std::str::from_utf8(&bytes[cursor..name_end])
                .map_err(|_| invalid(cursor, "custom section name is not UTF-8"))?;
            if name == MANIFEST_SECTION {
                if manifest.is_some() {
                    return Err(N2linkError::NotSupported(format!(
                        "WASM package has more than one {MANIFEST_SECTION}"
                    )));
                }
                let payload = &bytes[name_end..end];
                if payload.len() > MAX_MANIFEST_BYTES {
                    return Err(N2linkError::NotSupported(format!(
                        "{MANIFEST_SECTION} is {} bytes, more than {MAX_MANIFEST_BYTES}",
                        payload.len()
                    )));
                }
                let text =
                    std::str::from_utf8(payload).map_err(|_| invalid(name_end, "edgelink.manifest is not UTF-8"))?;
                manifest = Some(text.to_owned());
            }
        } else if id > 12 {
            return Err(invalid(start - 1, "unknown section id"));
        }
        offset = end;
    }
    manifest.ok_or_else(|| {
        N2linkError::NotSupported(format!(
            "WASM package has no {MANIFEST_SECTION} custom section (use `edgelinkd plugin pack`)"
        ))
    })
}

/// Append an `edgelink.manifest` custom section (what `edgelinkd plugin pack` does).
pub fn append_manifest(module: &[u8], manifest: &str) -> crate::Result<Vec<u8>> {
    if manifest.len() > MAX_MANIFEST_BYTES {
        return Err(N2linkError::NotSupported(format!("manifest is more than {MAX_MANIFEST_BYTES} bytes")));
    }
    if module.len() < 8 || &module[0..4] != MAGIC {
        return Err(invalid(0, "not a WebAssembly module"));
    }
    // Refuse to pack a module that already carries a manifest.
    if manifest_text(module, usize::MAX).is_ok() {
        return Err(N2linkError::NotSupported(format!("module already has an {MANIFEST_SECTION} section")));
    }
    let mut payload = Vec::new();
    write_u32(&mut payload, MANIFEST_SECTION.len() as u32);
    payload.extend_from_slice(MANIFEST_SECTION.as_bytes());
    payload.extend_from_slice(manifest.as_bytes());
    let mut out = module.to_vec();
    out.push(0);
    write_u32(&mut out, payload.len() as u32);
    out.extend_from_slice(&payload);
    Ok(out)
}

fn write_u32(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module() -> Vec<u8> {
        wat::parse_str("(module)").unwrap()
    }

    #[test]
    fn packed_manifest_is_found() {
        let packed = append_manifest(&module(), "id = 1").unwrap();
        assert_eq!(manifest_text(&packed, 1 << 20).unwrap(), "id = 1");
        assert!(append_manifest(&packed, "again").is_err());
    }

    #[test]
    fn framing_errors_are_precise() {
        let err = manifest_text(&module(), 1 << 20).unwrap_err().to_string();
        assert!(err.contains("no edgelink.manifest"), "{err}");
        assert!(manifest_text(b"\0asm", 1 << 20).unwrap_err().to_string().contains("not a WebAssembly"));
        let mut truncated = append_manifest(&module(), "x").unwrap();
        truncated.pop();
        assert!(manifest_text(&truncated, 1 << 20).unwrap_err().to_string().contains("past the end"));
        let mut forged = module();
        forged.extend_from_slice(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]);
        assert!(manifest_text(&forged, 1 << 20).is_err());
        let big = append_manifest(&module(), "x").unwrap();
        assert!(manifest_text(&big, 8).unwrap_err().to_string().contains("max_module_kib"));
    }

    #[test]
    fn duplicate_manifest_is_rejected() {
        let once = append_manifest(&module(), "a").unwrap();
        let mut twice = once.clone();
        twice.extend_from_slice(&once[8..]);
        assert!(manifest_text(&twice, 1 << 20).unwrap_err().to_string().contains("more than one"));
    }
}
