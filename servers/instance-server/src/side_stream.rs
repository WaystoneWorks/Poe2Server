//! The side channel's TCP stream on the instance port: the client hook's mod report.
//!
//! Connection (the first four bytes `PSUP` tell it from a game connection, whose first bytes
//! are an opcode): `b"PSUP"`, `u32 length`, a sealed frame (`Kind::ModReport`, payload a
//! [`poe_hook_shared::modreport::ModReport`]), answered by `b"PSUR"`, status, text. Frames are
//! authenticated with the session's keys ([`crate::sync::ClientReports::open_stream`]); the
//! frame size is capped before anything is read. The terrain upload that once shared this
//! stream is gone (the server generates every area itself); its frame kinds are refused.

use std::time::Duration;

use poe_hook_shared::sidechannel as sc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tracing::{info, warn};

use crate::sync::ClientReports;

/// Largest frame as sent (a mod report lists a few mods).
const MAX_FRAME: usize = 1 << 20;

/// Whole exchange deadline.
const DEADLINE: Duration = Duration::from_secs(10);

/// Whether a new connection starts with the side stream magic (peeked, nothing consumed).
pub(crate) async fn is_side_stream(stream: &TcpStream) -> bool {
    let mut magic = [0u8; 4];
    let peeking = async {
        loop {
            match stream.peek(&mut magic).await {
                Ok(n) if n >= magic.len() => return true,
                Ok(0) | Err(_) => return false,
                Ok(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), peeking).await.unwrap_or(false) && magic == sc::STREAM_MAGIC
}

/// Handles one side stream connection (the caller saw the `PSUP` magic with `peek`).
pub async fn serve(mut stream: TcpStream, reports: &ClientReports) -> std::io::Result<()> {
    tokio::time::timeout(DEADLINE, serve_frame(&mut stream, reports)).await.unwrap_or(Ok(()))
}

async fn serve_frame(stream: &mut TcpStream, reports: &ClientReports) -> std::io::Result<()> {
    let mut head = [0u8; 8];
    if stream.read_exact(&mut head).await.is_err() {
        return Ok(());
    }
    let len = u32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
    if head[..4] != sc::STREAM_MAGIC || len > MAX_FRAME || len < sc::HEADER_LEN + sc::TAG_LEN {
        return reply(stream, sc::REPLY_REJECTED, "bad side stream header").await;
    }
    let mut frame = vec![0u8; len];
    stream.read_exact(&mut frame[..sc::HEADER_LEN]).await?;
    if !sc::peek_session_header(&frame).is_some_and(|id| reports.knows(id)) {
        warn!("side stream from an unknown session");
        return reply(stream, sc::REPLY_AUTH, "unknown session").await;
    }
    stream.read_exact(&mut frame[sc::HEADER_LEN..]).await?;
    let (session, payload) = match reports.open_stream(&frame) {
        Ok(v) => v,
        Err(r) => {
            warn!(?r, "side stream frame rejected");
            return reply(stream, sc::REPLY_AUTH, &format!("{r:?}")).await;
        }
    };
    // The hook's list of loaded client mods: kept for the login check of this session.
    match poe_hook_shared::modreport::ModReport::decode(&payload) {
        Ok(report) => {
            info!(session = format_args!("{session:016x}"), mods = report.mods.len(), "client mod report");
            reports.store_mod_report(session, report);
            reply(stream, sc::REPLY_OK, "stored").await
        }
        Err(e) => {
            warn!("client mod report rejected: {e}");
            reply(stream, sc::REPLY_REJECTED, &e.to_string()).await
        }
    }
}

async fn reply(stream: &mut TcpStream, status: u8, text: &str) -> std::io::Result<()> {
    stream.write_all(&sc::encode_reply(status, text)).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc};

    use poe_hook_shared::sidechannel::{Kind, SessionKeys, seal};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    /// The hook's side of a side stream connection (`poe-hook-core` `side.rs`).
    async fn client_exchange(addr: SocketAddr, frame: &[u8]) -> (u8, String) {
        let mut tcp = TcpStream::connect(addr).await.unwrap();
        let mut msg = sc::STREAM_MAGIC.to_vec();
        msg.extend((frame.len() as u32).to_le_bytes());
        msg.extend(frame);
        tcp.write_all(&msg).await.unwrap();
        let mut head = [0u8; 7];
        tcp.read_exact(&mut head).await.unwrap();
        assert_eq!(head[..4], sc::STREAM_REPLY_MAGIC);
        let mut text = vec![0u8; u16::from_le_bytes([head[5], head[6]]) as usize];
        tcp.read_exact(&mut text).await.unwrap();
        (head[4], String::from_utf8(text).unwrap())
    }

    /// An instance-port stand-in: accepts connections, hands side stream ones to `serve`.
    async fn rig() -> (SocketAddr, Arc<ClientReports>) {
        let reports = Arc::new(ClientReports::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let r = reports.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let r = r.clone();
                tokio::spawn(async move {
                    if is_side_stream(&stream).await {
                        let _ = serve(stream, &r).await;
                    }
                });
            }
        });
        (addr, reports)
    }

    #[tokio::test]
    async fn a_mod_report_over_tcp_is_kept_for_its_session() {
        use poe_hook_shared::modreport::{ModReport, ReportedMod, sha256};
        let (addr, reports) = rig().await;
        let material = [9u8; 64];
        let sid = reports.register(&material, 50000);
        let keys = SessionKeys::derive(&material);
        assert_eq!(reports.mod_report(sid), None);
        let report = ModReport {
            mods: vec![ReportedMod { id: "hello-both".into(), version: "0.1.0".into(), manifest_sha256: sha256(b"t"), dll_sha256: Some(sha256(b"d")) }],
        };
        let frame = seal(&keys, Kind::ModReport, 1, &report.encode());
        assert_eq!(client_exchange(addr, &frame).await, (sc::REPLY_OK, "stored".to_owned()));
        assert_eq!(reports.mod_report(sid), Some(report.clone()));
        // The session's waiter sees it at once; a replay of the frame is refused.
        assert_eq!(reports.wait_mod_report(sid, Duration::from_secs(1)).await, Some(report));
        assert_eq!(client_exchange(addr, &frame).await.0, sc::REPLY_AUTH);
        // A report that does not decode is rejected and keeps nothing for the other session.
        let other = [10u8; 64];
        let oid = reports.register(&other, 50001);
        let bad = seal(&SessionKeys::derive(&other), Kind::ModReport, 1, &[1, 5, 0]);
        assert_eq!(client_exchange(addr, &bad).await.0, sc::REPLY_REJECTED);
        assert_eq!(reports.mod_report(oid), None);
        // No report in time: the waiter gives up (an old hook).
        assert_eq!(reports.wait_mod_report(oid, Duration::from_millis(250)).await, None);
    }

    #[tokio::test]
    async fn a_terrain_upload_offer_is_refused() {
        let (addr, reports) = rig().await;
        let material = [11u8; 64];
        reports.register(&material, 50002);
        let offer = seal(&SessionKeys::derive(&material), Kind::UploadOffer, 1, &7u32.to_le_bytes());
        assert_eq!(client_exchange(addr, &offer).await.0, sc::REPLY_AUTH);
    }

    #[tokio::test]
    async fn game_connections_are_not_side_streams() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            // A Hello: an opcode, then the ticket.
            s.write_all(&[0x01, 0x00, 0xde, 0xad, 0xbe, 0xef]).await.unwrap();
            s
        });
        let (stream, _) = listener.accept().await.unwrap();
        assert!(!is_side_stream(&stream).await);
        drop(client.await.unwrap());
    }
}
