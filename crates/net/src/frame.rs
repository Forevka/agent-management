//! Length-prefixed [`Envelope`] frames on a QUIC stream.

use anyhow::{Context, Result, bail};
use fleet_proto::{Envelope, MAX_FRAME_LEN};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, env: &Envelope) -> Result<()> {
    let bytes = env.encode()?;
    // encode() already enforces MAX_FRAME_LEN, which fits in a u32.
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    Ok(())
}

/// Reads one frame. Returns `Ok(None)` on a clean end of stream between frames.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Envelope>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    // Checked before allocating, so a hostile length cannot make us reserve gigabytes.
    if len > MAX_FRAME_LEN {
        bail!("peer sent a {len} byte frame (limit {MAX_FRAME_LEN})");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)
        .await
        .context("stream ended inside a frame")?;
    Ok(Some(Envelope::decode(&buf)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::Body;

    #[tokio::test]
    async fn frames_roundtrip_and_eof_is_clean() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let env = Envelope {
            seq: 1,
            ack: 0,
            body: Body::Heartbeat,
        };
        write_frame(&mut a, &env).await.unwrap();
        drop(a);
        assert_eq!(read_frame(&mut b).await.unwrap(), Some(env));
        assert_eq!(read_frame(&mut b).await.unwrap(), None);
    }

    #[tokio::test]
    async fn oversized_length_is_rejected_before_allocating() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
    }
}
