//! Google Cast v2 (TLS 8009, the device's self-signed certificate pinned):
//! the frames (a 4-byte length, then a small protobuf envelope around
//! JSON), and an announcement said through the Default Media Receiver.
//!
//! The device's TLS certificate is its own, regenerated now and then (daily,
//! at each restart): it cannot be pinned. What is pinned is its identity, the
//! certificate it presents through DeviceAuth (as supermomo31's Cast work
//! for #129 does it): each connection, it signs a fresh nonce and its TLS
//! certificate of the moment with that identity's key.
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

type Stream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

pub const PORT: u16 = 8009;
pub const SENDER: &str = "sender-moli";
pub const RECEIVER: &str = "receiver-0";
pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
pub const NS_AUTH: &str = "urn:x-cast:com.google.cast.tp.deviceauth";
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

/// Who answered, as DeviceAuth proved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The identity certificate's fingerprint: what is pinned.
    pub fingerprint: String,
    /// Its validity as written (not before, not after).
    pub valid: Option<(String, String)>,
    /// The TLS certificate's validity, for comparison.
    pub tls_valid: Option<(String, String)>,
}

/// A connection to the device, its identity checked against `identity`
/// (or, without one, the identity it presents taken as its own: the first
/// contact).
///
/// # Errors
/// The device is unreachable, or another device answered.
pub async fn connect(
    host: &str,
    port: u16,
    identity: Option<&str>,
) -> anyhow::Result<(Stream, Identity)> {
    // The TLS certificate changes by itself: DeviceAuth vouches for it.
    let tls = PinnedTls::new(None)?;
    let mut stream = tls.connect(host, port).await.context("cast")?;
    let cert = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .context("no TLS certificate from the Cast device")?
        .as_ref()
        .to_vec();
    let who = tokio::time::timeout(
        Duration::from_secs(5),
        authenticate(&mut stream, identity, &cert),
    )
    .await
    .context("Cast authentication timeout")??;
    Ok((stream, who))
}

/// DeviceAuth: a fresh nonce, the device's signature over it and over the
/// TLS certificate of the connection.
async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    pin: Option<&str>,
    tls_cert: &[u8],
) -> anyhow::Result<Identity> {
    use ring::rand::{SecureRandom as _, SystemRandom};
    let mut nonce = [0u8; 32];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("no nonce for the Cast authentication"))?;
    // DeviceAuthMessage { challenge { signature_algorithm: RSASSA_PKCS1v15,
    // sender_nonce, hash_algorithm: SHA256 } }, as a binary payload.
    let challenge = Writer::new()
        .msg(1, Writer::new().uint(1, 1).bytes(2, &nonce).uint(3, 1))
        .finish();
    let message = Writer::new()
        .uint(1, 0)
        .str(2, "sender-0")
        .str(3, RECEIVER)
        .str(4, NS_AUTH)
        .uint(5, 1)
        .bytes(7, &challenge)
        .finish();
    let mut frame = u32::try_from(message.len())?.to_be_bytes().to_vec();
    frame.extend_from_slice(&message);
    stream.write_all(&frame).await?;
    stream.flush().await?;
    for _ in 0..8 {
        let frame = read_frame(stream).await?;
        let m = parse(&frame).context("invalid Cast frame")?;
        if m.str(4) != Some(NS_AUTH) {
            continue;
        }
        let payload = m.bytes(7).context("no Cast authentication payload")?;
        return check_answer(payload, pin, &nonce, tls_cert);
    }
    bail!("no answer to the Cast authentication")
}

/// The device's answer to a challenge: its identity, once its signature
/// over `nonce` and `tls_cert` holds (against `pin`, or the identity it
/// presents at the first contact).
fn check_answer(
    payload: &[u8],
    pin: Option<&str>,
    nonce: &[u8],
    tls_cert: &[u8],
) -> anyhow::Result<Identity> {
    let answer = parse(payload).context("invalid Cast authentication answer")?;
    if answer.bytes(3).is_some() {
        bail!("the Cast device refused to authenticate");
    }
    let response = parse(answer.bytes(2).context("no Cast authentication response")?)
        .context("invalid Cast authentication response")?;
    // RSASSA_PKCS1v15 (1, the default), SHA-256 (1), our nonce: no replay,
    // no downgrade.
    if response.uint(4).unwrap_or(1) != 1
        || response.uint(6) != Some(1)
        || response.bytes(5) != Some(nonce)
    {
        bail!("Cast authentication requires SHA-256 and the current nonce");
    }
    let cert = response.bytes(2).context("no Cast device certificate")?;
    let signature = response.bytes(1).context("no Cast device signature")?;
    let fingerprint = crate::certificate_fingerprint(cert);
    let mut signed = nonce.to_vec();
    signed.extend_from_slice(tls_cert);
    crate::verify_cast_signature(cert, pin.unwrap_or(&fingerprint), &signed, signature)?;
    Ok(Identity {
        fingerprint,
        valid: crate::certificate_validity(cert),
        tls_valid: crate::certificate_validity(tls_cert),
    })
}

/// Says the sound at `url` (one the device fetches) at `level` (0–1), on a
/// connection [`connect`] opened, the device's own volume put back
/// afterwards, within `limit`. `started` as the device starts playing it.
///
/// # Errors
/// The device refused the app or the sound, or it did not end in time.
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

    #[test]
    fn an_authentication_answer_is_refused_when_missing_replayed_or_downgraded() {
        assert!(check_answer(&[], None, b"nonce", b"tls").is_err());
        let refused = Writer::new().msg(3, Writer::new().uint(1, 0)).finish();
        assert!(check_answer(&refused, None, b"nonce", b"tls").is_err());
        for (nonce, hash, algorithm) in [(b"old".as_slice(), 1, 1), (b"new", 0, 1), (b"new", 1, 2)]
        {
            let answer = Writer::new()
                .msg(
                    2,
                    Writer::new()
                        .bytes(1, b"signature")
                        .bytes(2, b"certificate")
                        .uint(4, algorithm)
                        .bytes(5, nonce)
                        .uint(6, hash),
                )
                .finish();
            let error = check_answer(&answer, None, b"new", b"tls").unwrap_err();
            assert!(error.to_string().contains("current nonce"));
        }
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    /// supermomo31's vectors (#129): an identity, its signature over a
    /// nonce and a TLS certificate.
    #[test]
    fn an_identity_vouches_for_the_tls_certificate_it_signed_only() {
        let cert = hex(
            "308202e8308201d0a0030201020214499e23948a2a70f7e2dc330c87cd82946bb4bd66300d06092a864886f70d01010b050030143112301006035504030c09636173742d74657374301e170d3236313130333230313730355a170d3236313130343230313730355a30143112301006035504030c09636173742d7465737430820122300d06092a864886f70d01010105000382010f003082010a0282010100985fd9c83a2feafc952f86d588266fe4f6f09735dd62205824b79da91c2cd5478c53927b0ec1ac818bf02d0ea6798e9c88b8412ea78779ff314c98e5ab226b51cdb0ac7a00626bb99cb04f3eeab5a5a053b396f06eb8c99a107f9c7a2ff790415842798c6d7a1a889a227d18824c36ec0f56b3c72ee73358fd9835afe83845ae07fe58fc9c785abfd5d9c58f079fea4526059c27d7818961767067d7b144c372a588dd6d16f718b69c0d898238a437952f9fba95a52dbe8e1b0160068ad1e304a00811f032d03930a4a28e81c43637ea213744630b494ebff508bc27f1bc33dbc1c37c9386bd979c0b793ed016fe19bcdf18d6911db5c3f36931d796fa79a4290203010001a3323030301d0603551d0e0416041418ac473870e4e21f36f03081c2d3eedfbb0efd78300f0603551d130101ff040530030101ff300d06092a864886f70d01010b050003820101005ac744724ed068091fe2025485abf42cd43a595185852423db0e9e497cac4b398194dc49389c203622130d0b41e5a4cfaab8856b5c7928439c42ac16522f7a970082574b96769def6a1f76a9e29975d895bdafcb9f689c2ac754356b4d288fb71d7436e25532673a101bca8758d918e07a7f1474410f9ce4b3dc29db8ade02e2eaffa53c16010e76f238e615e372f3e4e1845c919ceb238fdfbd6a7e8ccf4a337df6e088587733671b4f1b3c2da2ebc277216672d64ea61e65b43a9e92fb9640255d2bb0a40c9c35d0ed29b8c42887f000feb2dab2f50fac58a20f7a9174f6e90632d13c38faeb1c736171f2bd228ce4d564a722ecee1af062a5de080db7a391",
        );
        let signature = hex(
            "71e52e8a32857f9ba9f5bbbfe376780296d847116f05d6b70d7a623270f6f853646f73ca7ddd84a21853776d79e74b1e1ffc7218bb5ab5ed377bfbc48197b85cbf1ce97d676eebe8ea7690f76a44b0fb659fdfbc5600921b847406037ff8aa8ad13f9d3ebf2f85ed42bd811af543d26699b4e4fe189d6bde108d852d8140193801bea7114884f97bef91b6e1c1c81aca247d1686b339a6f3afa6b1234950e127e21cc25ff3520c13dc260e886905ba76e7256362f6f6e60a54d2ad75ca40b8c748d6d454db93e2d27a5f166dcf77c26bb9d261a8772618120dcb0ba78ff987237686f670465d82d44d99f545c8802f2fc1ac1ca5dca443e7d299a24d505916b6",
        );
        let answer = Writer::new()
            .msg(
                2,
                Writer::new()
                    .bytes(1, &signature)
                    .bytes(2, &cert)
                    .uint(4, 1)
                    .bytes(5, b"nonce:")
                    .uint(6, 1),
            )
            .finish();
        let first = check_answer(&answer, None, b"nonce:", b"tls-before").unwrap();
        assert_eq!(first.fingerprint, crate::certificate_fingerprint(&cert));
        assert_eq!(
            first.valid,
            Some(("261103201705Z".into(), "261104201705Z".into()))
        );
        let again = check_answer(&answer, Some(&first.fingerprint), b"nonce:", b"tls-before");
        assert_eq!(again.unwrap().fingerprint, first.fingerprint);
        // Another TLS certificate than the one signed; another identity.
        assert!(check_answer(&answer, None, b"nonce:", b"tls-after").is_err());
        assert!(check_answer(&answer, Some(&"0".repeat(64)), b"nonce:", b"tls-before").is_err());
    }

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
