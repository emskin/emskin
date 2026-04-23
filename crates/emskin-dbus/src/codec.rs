//! Length-prefixed JSON framing for the ctl-socket.
//!
//! Wire format: `[u32 LE: payload_len][payload: UTF-8 JSON]`. Same codec
//! emskin already uses for its Emacs IPC — chosen for consistency, not
//! because raw JSON is the fastest option (traffic is well under 10 KB/s
//! in practice, see CLAUDE.md).

use serde::{de::DeserializeOwned, Serialize};
use std::io::{self, Read, Write};

/// Reject payloads over 1 MiB. Defensive only — real ctl-socket messages
/// are < 1 KiB.
pub const MAX_FRAME_SIZE: usize = 1 << 20;

/// Encode `value` as JSON and write a length-prefixed frame.
pub fn write_frame<W: Write, T: Serialize + ?Sized>(writer: &mut W, value: &T) -> io::Result<()> {
    let payload =
        serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload exceeds MAX_FRAME_SIZE",
        ));
    }
    let len = (payload.len() as u32).to_le_bytes();
    writer.write_all(&len)?;
    writer.write_all(&payload)?;
    Ok(())
}

/// Read the next length-prefixed frame and decode it as `T`.
///
/// Distinguishes "clean EOF at a frame boundary" (`Ok(None)`, for the
/// peer politely closing the socket between messages) from "EOF mid-
/// frame" (`Err(UnexpectedEof)`, which is a protocol violation).
pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    let mut read = 0usize;
    while read < 4 {
        match reader.read(&mut len_buf[read..])? {
            0 if read == 0 => return Ok(None),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => read += n,
        }
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_SIZE",
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{EmskinToProxy, ProxyToEmskin};
    use std::io::Cursor;

    fn round_trip_emskin_to_proxy(msg: EmskinToProxy) {
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(buf);
        let decoded: EmskinToProxy = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn round_trip_focus_changed() {
        round_trip_emskin_to_proxy(EmskinToProxy::FocusChanged {
            ctx: 42,
            rect: [10, 20, 300, 400],
        });
    }

    #[test]
    fn round_trip_client_born() {
        round_trip_emskin_to_proxy(EmskinToProxy::ClientBorn { pid: 12345, ctx: 7 });
    }

    #[test]
    fn round_trip_bulk_rects() {
        round_trip_emskin_to_proxy(EmskinToProxy::BulkRects {
            updates: vec![(1, [0, 0, 100, 200]), (2, [300, 0, 100, 200])],
        });
    }

    #[test]
    fn round_trip_unit_variants() {
        round_trip_emskin_to_proxy(EmskinToProxy::FocusCleared);
        round_trip_emskin_to_proxy(EmskinToProxy::Shutdown);
    }

    #[test]
    fn round_trip_workspace_switched() {
        round_trip_emskin_to_proxy(EmskinToProxy::WorkspaceSwitched { workspace_id: 3 });
    }

    #[test]
    fn round_trip_ctx_gone() {
        round_trip_emskin_to_proxy(EmskinToProxy::CtxGone { ctx: 99 });
    }

    #[test]
    fn round_trip_rect_changed() {
        round_trip_emskin_to_proxy(EmskinToProxy::RectChanged {
            ctx: 5,
            rect: [100, 200, 800, 600],
        });
    }

    #[test]
    fn round_trip_proxy_to_emskin() {
        let msgs = [
            ProxyToEmskin::Ready,
            ProxyToEmskin::Error {
                context: "accept".into(),
                message: "permission denied".into(),
            },
            ProxyToEmskin::Stats {
                active_conns: 3,
                msg_rate: 42,
            },
        ];
        for msg in msgs {
            let mut buf = Vec::new();
            write_frame(&mut buf, &msg).unwrap();
            let mut cursor = Cursor::new(buf);
            let decoded: ProxyToEmskin = read_frame(&mut cursor).unwrap().unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn multiple_frames_same_buffer() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &EmskinToProxy::ClientBorn { pid: 1, ctx: 1 }).unwrap();
        write_frame(&mut buf, &EmskinToProxy::FocusCleared).unwrap();
        write_frame(&mut buf, &EmskinToProxy::Shutdown).unwrap();

        let mut cursor = Cursor::new(buf);
        assert_eq!(
            read_frame::<_, EmskinToProxy>(&mut cursor)
                .unwrap()
                .unwrap(),
            EmskinToProxy::ClientBorn { pid: 1, ctx: 1 }
        );
        assert_eq!(
            read_frame::<_, EmskinToProxy>(&mut cursor)
                .unwrap()
                .unwrap(),
            EmskinToProxy::FocusCleared
        );
        assert_eq!(
            read_frame::<_, EmskinToProxy>(&mut cursor)
                .unwrap()
                .unwrap(),
            EmskinToProxy::Shutdown
        );
    }

    #[test]
    fn clean_eof_at_frame_boundary_returns_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let v: Option<EmskinToProxy> = read_frame(&mut cursor).unwrap();
        assert!(v.is_none());
    }

    #[test]
    fn clean_eof_after_full_frame_returns_none() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &EmskinToProxy::Shutdown).unwrap();
        let mut cursor = Cursor::new(buf);
        let first: EmskinToProxy = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(first, EmskinToProxy::Shutdown);
        let next: Option<EmskinToProxy> = read_frame(&mut cursor).unwrap();
        assert!(next.is_none());
    }

    #[test]
    fn truncated_length_header_is_unexpected_eof() {
        let buf: Vec<u8> = vec![0x10, 0x00];
        let mut cursor = Cursor::new(buf);
        let err = read_frame::<_, EmskinToProxy>(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn truncated_payload_is_unexpected_eof() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(b"hello");
        let mut cursor = Cursor::new(buf);
        let err = read_frame::<_, EmskinToProxy>(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_length_rejected() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&((MAX_FRAME_SIZE as u32) + 1).to_le_bytes());
        let mut cursor = Cursor::new(buf);
        let err = read_frame::<_, EmskinToProxy>(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn malformed_json_payload_is_invalid_data() {
        let payload = b"not json at all";
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(payload);
        let mut cursor = Cursor::new(buf);
        let err = read_frame::<_, EmskinToProxy>(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
