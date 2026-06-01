//! Async length-prefixed msgpack framing over tokio streams — the same wire format as
//! [`protocol::codec`] (`[u32 BE len][rmp body]`), so the sync hook forwarder and the
//! async daemon/UI interoperate.

use protocol::codec::{self, CodecError};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Write one framed message.
pub async fn write_frame<W, T>(w: &mut W, value: &T) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let buf = codec::encode(value)?;
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// Read one framed message. Returns [`CodecError::Closed`] on clean EOF.
pub async fn read_frame<R, T>(r: &mut R) -> Result<T, CodecError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(CodecError::Closed),
        Err(e) => return Err(e.into()),
    }
    let n = u32::from_be_bytes(len);
    if n > codec::MAX_FRAME {
        return Err(CodecError::TooLarge(n));
    }
    let mut body = vec![0u8; n as usize];
    r.read_exact(&mut body).await?;
    Ok(rmp_serde::from_slice(&body)?)
}
