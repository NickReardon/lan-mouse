//! Clipboard protocol v1, separate from the fixed-size input datagrams.
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(super) const MAX_TEXT: usize = 1024 * 1024;
const MAGIC: &[u8; 8] = b"LMCLIP01";
pub(super) const PUSH: u8 = 1;
pub(super) const PULL: u8 = 2;

pub(super) async fn write_header(
    stream: &mut (impl AsyncWrite + Unpin),
    operation: u8,
    session: [u8; 16],
    sequence: u64,
) -> io::Result<()> {
    stream.write_all(MAGIC).await?;
    stream.write_u8(operation).await?;
    stream.write_all(&session).await?;
    stream.write_u64(sequence).await
}

pub(super) async fn read_header(
    stream: &mut (impl AsyncRead + Unpin),
) -> io::Result<(u8, [u8; 16], u64)> {
    let mut magic = [0; 8];
    stream.read_exact(&mut magic).await?;
    if &magic != MAGIC {
        return Err(invalid("unsupported clipboard protocol"));
    }
    let operation = stream.read_u8().await?;
    if !matches!(operation, PUSH | PULL) {
        return Err(invalid("invalid clipboard operation"));
    }
    let mut session = [0; 16];
    stream.read_exact(&mut session).await?;
    Ok((operation, session, stream.read_u64().await?))
}

pub(super) async fn write_text(
    stream: &mut (impl AsyncWrite + Unpin),
    text: Option<&str>,
) -> io::Result<()> {
    match text {
        None => stream.write_u32(u32::MAX).await,
        Some(text) => {
            if text.len() > MAX_TEXT {
                return Err(invalid("clipboard text exceeds 1 MiB"));
            }
            stream.write_u32(text.len() as u32).await?;
            stream.write_all(text.as_bytes()).await
        }
    }
}

pub(super) async fn read_text(stream: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<String>> {
    let len = stream.read_u32().await?;
    if len == u32::MAX {
        return Ok(None);
    }
    if len as usize > MAX_TEXT {
        return Err(invalid("clipboard text exceeds 1 MiB"));
    }
    let mut data = vec![0; len as usize];
    stream.read_exact(&mut data).await?;
    String::from_utf8(data)
        .map(Some)
        .map_err(|_| invalid("invalid clipboard UTF-8"))
}

pub(super) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unicode_empty_and_absent_round_trip() {
        for text in [Some("Hello\r\n世界 🐭"), Some(""), None] {
            let mut wire = Vec::new();
            write_header(&mut wire, PUSH, [7; 16], 42).await.unwrap();
            write_text(&mut wire, text).await.unwrap();
            let mut input = wire.as_slice();
            assert_eq!(read_header(&mut input).await.unwrap(), (PUSH, [7; 16], 42));
            assert_eq!(read_text(&mut input).await.unwrap().as_deref(), text);
        }
    }

    #[tokio::test]
    async fn rejects_oversized_truncated_invalid_and_unknown_messages() {
        let len = (MAX_TEXT as u32 + 1).to_be_bytes();
        assert!(read_text(&mut len.as_slice()).await.is_err());
        assert!(
            write_text(&mut Vec::new(), Some(&"x".repeat(MAX_TEXT + 1)))
                .await
                .is_err()
        );
        assert!(read_text(&mut b"\0\0\0\x02x".as_slice()).await.is_err());
        assert!(read_text(&mut b"\0\0\0\x01\xff".as_slice()).await.is_err());
        assert!(read_header(&mut b"LMCLIP02".as_slice()).await.is_err());
        assert!(read_header(&mut b"LMCLIP01\xff".as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn accepts_exact_payload_limit() {
        let text = "x".repeat(MAX_TEXT);
        let mut wire = Vec::new();
        write_text(&mut wire, Some(&text)).await.unwrap();
        assert_eq!(read_text(&mut wire.as_slice()).await.unwrap(), Some(text));
    }
}
