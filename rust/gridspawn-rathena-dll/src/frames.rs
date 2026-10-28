//! Pipe framing: u32 little-endian length, then UTF-8 JSON (at most 64 KiB).

use std::io::{self, Read, Write};

use serde_json::Value;

pub const MAX_FRAME: usize = 64 * 1024;

pub fn write_frame(out: &mut impl Write, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    out.write_all(&frame)?;
    out.flush()
}

/// `Ok(None)` at a clean end of the stream.
pub fn read_frame(input: &mut impl Read) -> io::Result<Option<Value>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip_and_refuse_oversize() {
        let mut buffer = Vec::new();
        let first = json!({"event": "ready", "version": "0.1.0"});
        write_frame(&mut buffer, &first).unwrap();
        write_frame(&mut buffer, &json!({"id": 7, "op": "attach"})).unwrap();
        let first_len = u32::from_le_bytes(buffer[..4].try_into().unwrap()) as usize;
        assert_eq!(first_len, serde_json::to_vec(&first).unwrap().len());
        let mut reader = &buffer[..];
        assert_eq!(read_frame(&mut reader).unwrap().unwrap()["event"], "ready");
        assert_eq!(read_frame(&mut reader).unwrap().unwrap()["id"], 7);
        assert!(read_frame(&mut reader).unwrap().is_none(), "clean end");
        let huge = json!({"x": "y".repeat(MAX_FRAME)});
        assert!(write_frame(&mut Vec::new(), &huge).is_err());
        let mut bad = (MAX_FRAME as u32 + 1).to_le_bytes().to_vec();
        bad.extend_from_slice(b"{}");
        assert!(read_frame(&mut &bad[..]).is_err());
    }
}
