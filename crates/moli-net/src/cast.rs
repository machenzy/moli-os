//! Google Cast v2 (TLS 8009, the device's self-signed certificate pinned):
//! the frames (a 4-byte length, then a small protobuf envelope around
//! JSON), and an announcement said through the Default Media Receiver.
//!
//! An announcement, as Home Assistant's `tts.speak` and Gladys do it: the
//! receiver's volume is read, the announcement's level set, the Default
//! Media Receiver launched (what played stops), the sound loaded from the
//! address Moli publishes it at, followed until it is over, then the app
//! stopped and the volume put back.

use std::time::Duration;

use anyhow::{Context as _, bail};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;

use crate::PinnedTls;
use crate::proto::{Writer, parse};

pub const PORT: u16 = 8009;
pub const SENDER: &str = "sender-moli";
pub const RECEIVER: &str = "receiver-0";
pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
/// Google's Default Media Receiver: plays a URL, on every Cast device.
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";
const MAX_MESSAGE: usize = 256 * 1024;
/// A heartbeat this often while an announcement plays.
const PING_EVERY: Duration = Duration::from_secs(5);

/// A message's sender, namespace and JSON payload.
#[must_use]
pub fn envelope(frame: &[u8]) -> Option<(String, String, Json)> {
    let m = parse(frame)?;
    let body = serde_json::from_str(m.str(6)?).ok()?;
    Some((m.str(2)?.to_owned(), m.str(4)?.to_owned(), body))
}

/// Sends `payload` to `destination` on `namespace`.
///
/// # Errors
/// The connection failed.
pub async fn send<W: AsyncWrite + Unpin>(
    write: &mut W,
    destination: &str,
    namespace: &str,
    payload: &Json,
) -> anyhow::Result<()> {
    let message = Writer::new()
        .uint(1, 0)
        .str(2, SENDER)
        .str(3, destination)
        .str(4, namespace)
        .uint(5, 0)
        .str(6, &payload.to_string())
        .finish();
    let len = u32::try_from(message.len()).context("message too long")?;
    let mut frame = len.to_be_bytes().to_vec();
    frame.extend_from_slice(&message);
    write.write_all(&frame).await?;
    write.flush().await?;
    Ok(())
}

/// The next frame.
///
/// # Errors
/// The connection failed, or the device sent more than a message can hold.
pub async fn read_frame<R: AsyncRead + Unpin>(read: &mut R) -> anyhow::Result<Vec<u8>> {
    let len = usize::try_from(read.read_u32().await?)?;
    if len > MAX_MESSAGE {
        bail!("message of {len} bytes from the Cast device");
    }
    let mut frame = vec![0; len];
    read.read_exact(&mut frame).await?;
    Ok(frame)
}

/// Says the sound at `url` (one the device fetches) at `level` (0–1), the
/// device's own volume put back afterwards, within `limit`. `started` as
/// the device starts playing it.
///
/// # Errors
/// The device is unreachable, refused the app or the sound, or the sound
/// did not end in time.
pub async fn say(
    tls: &PinnedTls,
    host: &str,
    port: u16,
    url: &str,
    level: f64,
    limit: Duration,
    started: impl FnMut() + Send,
) -> anyhow::Result<()> {
    let stream = tls.connect(host, port).await.context("cast")?;
    say_on(stream, url, level, limit, started).await
}

/// [`say`] on a connection already open.
///
/// # Errors
/// As [`say`].
pub async fn say_on<S: AsyncRead + AsyncWrite + Send + 'static>(
    stream: S,
    url: &str,
    level: f64,
    limit: Duration,
    started: impl FnMut() + Send,
) -> anyhow::Result<()> {
    let (mut read, write) = tokio::io::split(stream);
    let (frames_tx, frames) = mpsc::channel::<Vec<u8>>(16);
    let reader = tokio::spawn(async move {
        while let Ok(frame) = read_frame(&mut read).await {
            if frames_tx.send(frame).await.is_err() {
                return;
            }
        }
    });
    let mut line = Line {
        write,
        frames,
        request: 0,
    };
    let mut undo = Undo::default();
    let result = tokio::time::timeout(limit, announce(&mut line, &mut undo, url, level, started))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the announcement did not end in time")));
    // Whatever happened, the app stops and the volume comes back.
    if tokio::time::timeout(Duration::from_secs(5), line.put_back(&undo))
        .await
        .is_err()
    {
        tracing::warn!("cast device not put back in time");
    }
    reader.abort();
    result
}

/// What an announcement leaves to undo.
#[derive(Debug, Default)]
struct Undo {
    /// The device's volume before, 0–1.
    volume: Option<f64>,
    /// The Default Media Receiver's session, once launched.
    session: Option<String>,
}

struct Line<W> {
    write: W,
    frames: mpsc::Receiver<Vec<u8>>,
    request: u64,
}

impl<W: AsyncWrite + Unpin> Line<W> {
    async fn send(&mut self, to: &str, namespace: &str, mut payload: Json) -> anyhow::Result<u64> {
        self.request += 1;
        if payload
            .get("type")
            .is_some_and(|t| t != "CONNECT" && t != "PING" && t != "PONG")
        {
            payload["requestId"] = json!(self.request);
        }
        send(&mut self.write, to, namespace, &payload).await?;
        Ok(self.request)
    }

    /// The next message other than a heartbeat (answered), with a
    /// heartbeat of ours while waiting.
    async fn next(&mut self) -> anyhow::Result<(String, String, Json)> {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    send(&mut self.write, RECEIVER, NS_HEARTBEAT, &json!({ "type": "PING" })).await?;
                }
                frame = self.frames.recv() => {
                    let frame = frame.context("the Cast device closed the connection")?;
                    let Some((source, namespace, body)) = envelope(&frame) else { continue };
                    if namespace == NS_HEARTBEAT {
                        if body["type"] == "PING" {
                            send(&mut self.write, &source, NS_HEARTBEAT, &json!({ "type": "PONG" })).await?;
                        }
                        continue;
                    }
                    return Ok((source, namespace, body));
                }
            }
        }
    }

    /// Stops the app and puts the volume back (failures only warned: the
    /// announcement was heard or not, nothing changes it now).
    async fn put_back(&mut self, undo: &Undo) {
        if let Some(session) = &undo.session
            && let Err(e) = self
                .send(
                    RECEIVER,
                    NS_RECEIVER,
                    json!({ "type": "STOP", "sessionId": session }),
                )
                .await
        {
            tracing::warn!(error = %format!("{e:#}"), "cast app not stopped");
        }
        if let Some(volume) = undo.volume
            && let Err(e) = self
                .send(
                    RECEIVER,
                    NS_RECEIVER,
                    json!({ "type": "SET_VOLUME", "volume": { "level": volume } }),
                )
                .await
        {
            tracing::warn!(error = %format!("{e:#}"), "cast volume not put back");
        }
    }
}

/// The announcement itself, noting in `undo` what to put back as it goes
/// (it may stop midway).
async fn announce<W: AsyncWrite + Unpin>(
    line: &mut Line<W>,
    undo: &mut Undo,
    url: &str,
    level: f64,
    mut started: impl FnMut() + Send,
) -> anyhow::Result<()> {
    line.send(RECEIVER, NS_CONNECTION, json!({ "type": "CONNECT" }))
        .await?;
    let asked = line
        .send(RECEIVER, NS_RECEIVER, json!({ "type": "GET_STATUS" }))
        .await?;
    let status = receiver_status(line, Some(asked)).await?;
    undo.volume = status["volume"]["level"].as_f64();
    line.send(
        RECEIVER,
        NS_RECEIVER,
        json!({ "type": "SET_VOLUME", "volume": { "level": level.clamp(0.0, 1.0) } }),
    )
    .await?;
    line.send(
        RECEIVER,
        NS_RECEIVER,
        json!({ "type": "LAUNCH", "appId": DEFAULT_MEDIA_RECEIVER }),
    )
    .await?;
    let (transport, session) = launched(line).await?;
    undo.session = Some(session);
    line.send(&transport, NS_CONNECTION, json!({ "type": "CONNECT" }))
        .await?;
    line.send(
        &transport,
        NS_MEDIA,
        json!({
            "type": "LOAD",
            "autoplay": true,
            "media": {
                "contentId": url,
                "contentType": "audio/wav",
                "streamType": "BUFFERED",
                "metadata": { "metadataType": 0, "title": "Moli" },
            },
        }),
    )
    .await?;
    played(line, &transport, &mut started).await
}

/// The receiver's status (the answer to `asked`, or the next one).
async fn receiver_status<W: AsyncWrite + Unpin>(
    line: &mut Line<W>,
    asked: Option<u64>,
) -> anyhow::Result<Json> {
    loop {
        let (_, namespace, body) = line.next().await?;
        if namespace != NS_RECEIVER {
            continue;
        }
        match body["type"].as_str() {
            Some("RECEIVER_STATUS")
                if asked.is_none_or(|id| {
                    body["requestId"].as_u64().is_none_or(|r| r == id || r == 0)
                }) =>
            {
                return Ok(body["status"].clone());
            }
            Some("LAUNCH_ERROR" | "INVALID_REQUEST") => {
                bail!(
                    "the Cast device refused: {}",
                    body["reason"].as_str().unwrap_or("?")
                );
            }
            _ => {}
        }
    }
}

/// The Default Media Receiver once it runs: its transport and session.
async fn launched<W: AsyncWrite + Unpin>(line: &mut Line<W>) -> anyhow::Result<(String, String)> {
    loop {
        let status = receiver_status(line, None).await?;
        if let Some(app) = status["applications"]
            .as_array()
            .and_then(|apps| apps.iter().find(|a| a["appId"] == DEFAULT_MEDIA_RECEIVER))
            && let (Some(transport), Some(session)) =
                (app["transportId"].as_str(), app["sessionId"].as_str())
        {
            return Ok((transport.to_owned(), session.to_owned()));
        }
    }
}

/// Follows the sound until it is over.
async fn played<W: AsyncWrite + Unpin>(
    line: &mut Line<W>,
    transport: &str,
    started: &mut (impl FnMut() + Send),
) -> anyhow::Result<()> {
    let mut playing = false;
    loop {
        let (source, namespace, body) = line.next().await?;
        if namespace == NS_CONNECTION && source == transport && body["type"] == "CLOSE" {
            bail!("the Cast device closed the app before the end");
        }
        if namespace != NS_MEDIA {
            continue;
        }
        match body["type"].as_str() {
            Some("LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST" | "INVALID_PLAYER_STATE") => {
                bail!(
                    "the Cast device did not play the sound ({})",
                    body["type"].as_str().unwrap_or("?")
                );
            }
            Some("MEDIA_STATUS") => {}
            _ => continue,
        }
        let Some(status) = body["status"].as_array().and_then(|s| s.first()) else {
            // No media any more: over, if it ever started.
            if playing {
                return Ok(());
            }
            continue;
        };
        match (
            status["playerState"].as_str(),
            status["idleReason"].as_str(),
        ) {
            (Some("PLAYING"), _) => {
                if !playing {
                    playing = true;
                    started();
                }
            }
            (Some("IDLE"), Some("FINISHED")) => return Ok(()),
            (Some("IDLE"), Some(reason)) => {
                bail!("the Cast device stopped the sound ({reason})");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn envelopes_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        send(&mut a, RECEIVER, NS_MEDIA, &json!({ "type": "PING" }))
            .await
            .unwrap();
        let frame = read_frame(&mut b).await.unwrap();
        let m = parse(&frame).unwrap();
        assert_eq!(m.str(3), Some(RECEIVER));
        assert_eq!(m.str(4), Some(NS_MEDIA));
        let (source, namespace, body) = envelope(&frame).unwrap();
        assert_eq!((source.as_str(), namespace.as_str()), (SENDER, NS_MEDIA));
        assert_eq!(body["type"], "PING");
    }

    /// A device's side: answers as a Home Mini does, and records what it
    /// was asked (namespace, payload). `end`: how the sound ends.
    async fn device(stream: tokio::io::DuplexStream, end: &'static str) -> Vec<(String, Json)> {
        let (mut read, mut write) = tokio::io::split(stream);
        let mut asked = Vec::new();
        let mut volume = 0.6;
        let tell = |to: &str, ns: &str, body: Json| {
            let message = Writer::new()
                .uint(1, 0)
                .str(2, to)
                .str(3, SENDER)
                .str(4, ns)
                .uint(5, 0)
                .str(6, &body.to_string())
                .finish();
            let mut frame = u32::try_from(message.len()).unwrap().to_be_bytes().to_vec();
            frame.extend_from_slice(&message);
            frame
        };
        while let Ok(frame) = read_frame(&mut read).await {
            let (_, ns, body) = envelope(&frame).unwrap();
            let id = body["requestId"].clone();
            asked.push((ns.clone(), body.clone()));
            let answers: Vec<Vec<u8>> = match body["type"].as_str().unwrap_or_default() {
                "GET_STATUS" => vec![tell(
                    RECEIVER,
                    NS_RECEIVER,
                    json!({
                    "type": "RECEIVER_STATUS", "requestId": id,
                    "status": { "volume": { "level": volume, "muted": false } } }),
                )],
                "SET_VOLUME" => {
                    volume = body["volume"]["level"].as_f64().unwrap();
                    vec![tell(
                        RECEIVER,
                        NS_RECEIVER,
                        json!({
                        "type": "RECEIVER_STATUS", "requestId": id,
                        "status": { "volume": { "level": volume } } }),
                    )]
                }
                "LAUNCH" => vec![
                    tell(RECEIVER, NS_HEARTBEAT, json!({ "type": "PING" })),
                    tell(
                        RECEIVER,
                        NS_RECEIVER,
                        json!({
                        "type": "RECEIVER_STATUS", "requestId": id,
                        "status": { "volume": { "level": volume }, "applications": [{
                            "appId": DEFAULT_MEDIA_RECEIVER, "transportId": "web-7",
                            "sessionId": "s-1", "displayName": "Default Media Receiver" }] } }),
                    ),
                ],
                "LOAD" => {
                    let mut said = vec![
                        tell(
                            "web-7",
                            NS_MEDIA,
                            json!({ "type": "MEDIA_STATUS", "status": [
                            { "playerState": "IDLE", "mediaSessionId": 1 }] }),
                        ),
                        tell(
                            "web-7",
                            NS_MEDIA,
                            json!({ "type": "MEDIA_STATUS", "requestId": id,
                            "status": [{ "playerState": "PLAYING", "mediaSessionId": 1 }] }),
                        ),
                    ];
                    said.push(match end {
                        "load_failed" => tell("web-7", NS_MEDIA, json!({ "type": "LOAD_FAILED", "requestId": id })),
                        reason => tell("web-7", NS_MEDIA, json!({ "type": "MEDIA_STATUS", "status": [
                            { "playerState": "IDLE", "idleReason": reason, "mediaSessionId": 1 }] })),
                    });
                    said
                }
                "STOP" => {
                    vec![]
                }
                _ => vec![],
            };
            for a in answers {
                if write.write_all(&a).await.is_err() {
                    return asked;
                }
            }
            if body["type"] == "SET_VOLUME" && asked.iter().any(|(_, b)| b["type"] == "STOP") {
                return asked;
            }
        }
        asked
    }

    fn kinds(asked: &[(String, Json)]) -> Vec<String> {
        asked
            .iter()
            .filter(|(ns, _)| ns != NS_HEARTBEAT)
            .map(|(_, b)| b["type"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn an_announcement_is_said_then_the_device_put_back() {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let device = tokio::spawn(device(theirs, "FINISHED"));
        let mut starts = 0;
        say_on(
            ours,
            "http://moli/api/media/x.wav",
            0.35,
            Duration::from_secs(5),
            || starts += 1,
        )
        .await
        .unwrap();
        let asked = device.await.unwrap();
        assert_eq!(starts, 1);
        assert_eq!(
            kinds(&asked),
            [
                "CONNECT",
                "GET_STATUS",
                "SET_VOLUME",
                "LAUNCH",
                "CONNECT",
                "LOAD",
                "STOP",
                "SET_VOLUME"
            ]
        );
        let volumes: Vec<f64> = asked
            .iter()
            .filter(|(_, b)| b["type"] == "SET_VOLUME")
            .map(|(_, b)| b["volume"]["level"].as_f64().unwrap())
            .collect();
        assert_eq!(volumes, [0.35, 0.6], "announced at 35 %, back to 60 %");
        let load = &asked.iter().find(|(_, b)| b["type"] == "LOAD").unwrap().1;
        assert_eq!(load["media"]["contentId"], "http://moli/api/media/x.wav");
        assert_eq!(load["media"]["contentType"], "audio/wav");
        let stop = &asked.iter().find(|(_, b)| b["type"] == "STOP").unwrap().1;
        assert_eq!(stop["sessionId"], "s-1");
    }

    #[tokio::test]
    async fn a_sound_refused_or_cut_is_an_error_and_the_device_still_put_back() {
        for end in ["load_failed", "ERROR", "INTERRUPTED"] {
            let (ours, theirs) = tokio::io::duplex(64 * 1024);
            let device = tokio::spawn(device(theirs, end));
            let said = say_on(
                ours,
                "http://moli/x.wav",
                0.4,
                Duration::from_secs(5),
                || {},
            )
            .await;
            assert!(said.is_err(), "{end}");
            let asked = device.await.unwrap();
            let tail: Vec<String> = kinds(&asked).into_iter().rev().take(2).collect();
            assert_eq!(tail, ["SET_VOLUME", "STOP"], "{end}");
        }
    }

    #[tokio::test]
    async fn a_silent_device_is_given_up_in_time() {
        let (ours, _theirs) = tokio::io::duplex(64 * 1024);
        let said = say_on(
            ours,
            "http://moli/x.wav",
            0.4,
            Duration::from_millis(200),
            || {},
        )
        .await;
        assert!(format!("{:#}", said.unwrap_err()).contains("in time"));
    }
}
