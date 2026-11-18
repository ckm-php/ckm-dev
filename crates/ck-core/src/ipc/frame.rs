//! Framing: a 4-byte big-endian length, then that many bytes of JSON.
//!
//! The length is checked against the limit before the body is read.

use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Error, Result};

/// Bytes in the length prefix.
const HEADER_BYTES: usize = 4;

/// Writes one frame. A body over `limit` bytes gives
/// [`Error::FrameTooLarge`](crate::Error::FrameTooLarge) and writes nothing.
pub async fn write_frame<W, T>(writer: &mut W, message: &T, limit: u32) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(message)
        .map_err(|e| Error::Internal(format!("cannot encode a message: {e}")))?;
    let len = match u32::try_from(body.len()) {
        Ok(len) if len <= limit => len,
        Ok(len) => return Err(Error::FrameTooLarge { len, limit }),
        Err(_) => {
            return Err(Error::FrameTooLarge {
                len: u32::MAX,
                limit,
            });
        }
    };

    // One buffer, so the length and the body leave in a single write.
    let mut frame = Vec::with_capacity(HEADER_BYTES + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    writer
        .write_all(&frame)
        .await
        .map_err(|e| Error::os("send a message", e))?;
    writer
        .flush()
        .await
        .map_err(|e| Error::os("send a message", e))
}

/// Reads one frame.
///
/// - `Ok(None)`: the peer closed cleanly before the first byte of a frame.
/// - [`Error::FrameTooLarge`](crate::Error::FrameTooLarge): the length is over
///   `limit`; the body is not read.
/// - [`Error::FrameTruncated`](crate::Error::FrameTruncated): the stream ended
///   inside the length or the body.
/// - [`Error::Malformed`](crate::Error::Malformed): the body is not a `T`.
pub async fn read_frame<R, T>(reader: &mut R, limit: u32) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    // `read_exact` cannot tell "nothing came" from "part of the length came",
    // so the length is read in a loop.
    let mut header = [0u8; HEADER_BYTES];
    let mut filled = 0;
    while filled < HEADER_BYTES {
        let read = reader
            .read(&mut header[filled..])
            .await
            .map_err(|e| Error::os("read a message", e))?;
        if read == 0 {
            return if filled == 0 {
                Ok(None)
            } else {
                Err(Error::FrameTruncated)
            };
        }
        filled += read;
    }

    let len = u32::from_be_bytes(header);
    if len > limit {
        return Err(Error::FrameTooLarge { len, limit });
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::FrameTruncated
        } else {
            Error::os("read a message", e)
        }
    })?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(Error::Malformed)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[tokio::test]
    async fn a_frame_is_a_big_endian_length_then_json() {
        let message = json!({"op": "status"});
        let mut wire = Vec::new();
        write_frame(&mut wire, &message, 64).await.unwrap();

        let body = br#"{"op":"status"}"#;
        let len = u32::try_from(body.len()).unwrap();
        assert_eq!(wire[..HEADER_BYTES], len.to_be_bytes());
        assert_eq!(&wire[HEADER_BYTES..], body);

        let mut input = wire.as_slice();
        let read: Option<Value> = read_frame(&mut input, 64).await.unwrap();
        assert_eq!(read, Some(message.clone()));

        // A body over the limit writes nothing.
        let mut nothing = Vec::new();
        let result = write_frame(&mut nothing, &message, len - 1).await;
        assert!(matches!(result, Err(Error::FrameTooLarge { .. })));
        assert!(nothing.is_empty());
    }

    #[tokio::test]
    async fn an_oversized_length_is_rejected_before_the_body_is_read() {
        let mut wire = 1000u32.to_be_bytes().to_vec();
        wire.resize(HEADER_BYTES + 1000, b' ');

        let mut input = wire.as_slice();
        let result = read_frame::<_, Value>(&mut input, 999).await;
        assert!(matches!(
            result,
            Err(Error::FrameTooLarge {
                len: 1000,
                limit: 999
            })
        ));
        assert_eq!(input.len(), 1000, "the body must stay unread");
    }

    #[tokio::test]
    async fn a_clean_end_is_none_and_a_cut_frame_is_an_error() {
        let mut empty: &[u8] = &[];
        let read = read_frame::<_, Value>(&mut empty, 64).await.unwrap();
        assert!(read.is_none());

        let mut cut_length: &[u8] = &[0, 0];
        let result = read_frame::<_, Value>(&mut cut_length, 64).await;
        assert!(matches!(result, Err(Error::FrameTruncated)));

        let mut wire = 10u32.to_be_bytes().to_vec();
        wire.extend_from_slice(br#"{"a""#);
        let mut cut_body = wire.as_slice();
        let result = read_frame::<_, Value>(&mut cut_body, 64).await;
        assert!(matches!(result, Err(Error::FrameTruncated)));
    }
}
