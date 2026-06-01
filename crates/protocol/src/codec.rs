//! Length-prefixed msgpack framing: `[u32 BE length][rmp-serde body]`.
//!
//! Sync helpers are provided (used by the hook forwarder and tests). The daemon/UI
//! wrap their async sockets but reuse [`encode`] and the same length discipline.

use serde::{de::DeserializeOwned, Serialize};
use std::io::{Read, Write};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("frame too large: {0} bytes")]
    TooLarge(u32),
    #[error("connection closed")]
    Closed,
}

/// Hard cap so a corrupt length can't make us allocate the world.
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

/// Serialize `value` into a complete framed message (length prefix + body).
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let body = rmp_serde::to_vec_named(value)?;
    let len = body.len() as u32;
    if len > MAX_FRAME {
        return Err(CodecError::TooLarge(len));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Blocking write of one framed message.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> Result<(), CodecError> {
    let buf = encode(value)?;
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

/// Blocking read of one framed message.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, CodecError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(CodecError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(CodecError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok(rmp_serde::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Frame, HookEnvelope, HookKind, SessionId};

    #[test]
    fn roundtrips_a_frame_over_a_pipe() {
        let env = HookEnvelope {
            session: SessionId(7),
            kind: HookKind::PreToolUse,
            payload_json: r#"{"tool_name":"Bash"}"#.into(),
        };
        let frame = Frame::Hook(env);
        let bytes = encode(&frame).unwrap();
        // length prefix matches body
        let declared = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert_eq!(declared as usize, bytes.len() - 4);

        let mut cursor = std::io::Cursor::new(bytes);
        let decoded: Frame = read_frame(&mut cursor).unwrap();
        match decoded {
            Frame::Hook(h) => {
                assert_eq!(h.session, SessionId(7));
                assert_eq!(h.kind, HookKind::PreToolUse);
            }
            _ => panic!("wrong frame"),
        }
    }
}
