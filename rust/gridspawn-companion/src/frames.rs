//! Pipe framing (async): u32 little-endian length, then UTF-8 JSON, at most
//! 64 KiB. The same as `gridspawn-rathena-dll`'s frames.

use std::io;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u64 = 1;
pub const MAX_FRAME: usize = 64 * 1024;

pub async fn write_frame(out: &mut (impl AsyncWrite + Unpin), value: &Value) -> io::Result<()> {
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
    out.write_all(&frame).await?;
    out.flush().await
}

/// `Ok(None)` at a clean end of the stream.
pub async fn read_frame(input: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Value>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
            ) =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Equal-time comparison of two tokens.
pub fn same_token(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn frames_round_trip() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &json!({"hello": 1, "token": "ab"}))
            .await
            .unwrap();
        let mut reader = &buffer[..];
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap()["hello"], 1);
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[test]
    fn tokens_compare() {
        assert!(same_token("abcd", "abcd"));
        assert!(!same_token("abcd", "abce"));
        assert!(!same_token("abcd", "abc"));
    }
}
