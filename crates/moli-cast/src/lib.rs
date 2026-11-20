//! The `cast` kind: spoken announcements on a Google Cast speaker (Google
//! Home, Nest Mini, Nest Audio). Announcements only: no music. The queue,
//! the silence, the level and the speech are `moli-announce`'s; the
//! protocol is `moli_net::cast`.
//!
//! The speaker fetches the sound from Moli (`media_base`), through Google's
//! Default Media Receiver: what it was playing stops, as with Home
//! Assistant's `tts.speak`. Its volume is set for the announcement, then
//! put back. Its certificate is pinned at first contact (secret `cast_cert`).
//! Whether it is there is asked by a bare TCP connection, which cuts nothing.

use std::sync::Arc;
use std::time::Duration;

use moli_announce::{Announcements, Announcer, Speaker, Speech};
use moli_core::{Device, DeviceId};
use moli_net::PinnedTls;
use moli_runtime::{BoxFuture, Driver, DriverCtx, MediaPublisher};
use serde::Deserialize;

/// Whether the speaker is there, asked this often (a TCP connection only).
const REACH_EVERY: Duration = Duration::from_secs(30);
/// Where the speaker's certificate fingerprint is kept.
const CERT: &str = "cast_cert";
/// Time for the app to start and the sound to be fetched, beyond its length.
const SLACK: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Moli as the speaker reaches it (`http://192.168.1.97:8790`): the
    /// speaker fetches the sound from there.
    pub media_base: String,
    /// The Wyoming speech server (Piper), `host:port`.
    #[serde(default = "moli_announce::default_tts")]
    pub tts: String,
    /// Piper voice (default: the server's).
    #[serde(default)]
    pub voice: Option<String>,
    /// Where `announce_volume` starts, 0–100; once written, the point is kept.
    #[serde(default = "default_volume")]
    pub volume: u8,
    /// Says nothing during the house's quiet hours (`[guard.quiet_hours]`).
    #[serde(default)]
    pub quiet_hours: bool,
}

fn default_port() -> u16 {
    moli_net::cast::PORT
}

fn default_volume() -> u8 {
    50
}

impl Config {
    fn speech(&self) -> Speech {
        Speech {
            tts: self.tts.clone(),
            voice: self.voice.clone(),
        }
    }
}

#[derive(Debug)]
pub struct Cast {
    config: Config,
}

impl Cast {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl Driver for Cast {
    fn kind(&self) -> &'static str {
        "cast"
    }

    fn run<'a>(&'a self, ctx: &'a mut DriverCtx) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(run(&self.config, ctx))
    }
}

/// The speaker as `moli-announce` sees it: one that fetches the sound.
#[derive(Debug)]
struct Loudspeaker {
    host: String,
    port: u16,
    media_base: String,
    tls: Arc<PinnedTls>,
    publisher: MediaPublisher,
}

impl Speaker for Loudspeaker {
    fn play<'a>(
        &'a self,
        wav: Vec<u8>,
        level: u8,
        started: &'a moli_announce::Started,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let limit = length(&wav) + SLACK;
            let url = moli_announce::publish_sound(&self.publisher, &self.media_base, wav)?;
            moli_net::cast::say(
                &self.tls,
                &self.host,
                self.port,
                &url,
                f64::from(level) / 100.0,
                limit,
                || started.now(),
            )
            .await
        })
    }
}

/// How long a WAV plays, from its header (a minute when it does not say).
fn length(wav: &[u8]) -> Duration {
    let rate = wav
        .get(28..32)
        .filter(|_| wav.starts_with(b"RIFF"))
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .filter(|r| *r > 0);
    match rate {
        Some(bytes_per_second) => {
            let data = u64::try_from(wav.len().saturating_sub(44)).unwrap_or(u64::MAX);
            Duration::from_millis(data.saturating_mul(1000) / u64::from(bytes_per_second))
        }
        None => Duration::from_secs(60),
    }
}

async fn run(config: &Config, ctx: &mut DriverCtx) -> anyhow::Result<()> {
    let host = config.host.as_str();
    let id = ctx.device_id(host);
    let mut stored = ctx.secret(CERT);
    let tls = Arc::new(PinnedTls::new(stored.clone())?);
    let speaker = Loudspeaker {
        host: host.to_owned(),
        port: config.port,
        media_base: config.media_base.clone(),
        tls: Arc::clone(&tls),
        publisher: ctx.media_publisher(),
    };
    let mut announcements = Announcements::new(Some(Announcer::new(
        speaker,
        config.speech(),
        config.volume,
        ctx.media_publisher(),
    )))
    .keeping_quiet_hours(config.quiet_hours);
    ctx.upsert_device(device(ctx, &id, host, &announcements));
    announcements.start(ctx, &id);
    // The certificate, pinned now when the speaker is there (otherwise at
    // the first announcement).
    if stored.is_none() {
        if let Err(e) = tls.connect(host, config.port).await {
            tracing::info!(host, error = %format!("{e:#}"), "cast speaker not reached at start");
        }
        keep_pin(ctx, &tls, &mut stored).await;
    }
    let mut online = None;
    ctx.ready();
    let mut tick = tokio::time::interval(REACH_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            said = announcements.speaking_changed() => {
                Announcements::show_speaking(ctx, &id, said);
            }
            command = ctx.next_command() => {
                let Some(command) = command else { return Ok(()) };
                let result = announcements.command(ctx, &id, &command).unwrap_or_else(|| {
                    Err(moli_i18n::tr!("pilotes.cast.ne_se_regle_pas", point = &*command.key))
                });
                command.reply(result);
            }
            _ = tick.tick() => {
                announcements.tick(ctx, &id);
                let up = reachable(host, config.port).await;
                if online != Some(up) {
                    online = Some(up);
                    ctx.set_availability(&id, up);
                }
                if stored.is_none() {
                    keep_pin(ctx, &tls, &mut stored).await;
                }
            }
        }
    }
}

/// Keeps the certificate seen first, for the next starts.
async fn keep_pin(ctx: &DriverCtx, tls: &PinnedTls, stored: &mut Option<String>) {
    let Some(seen) = tls.pinned_fingerprint() else {
        return;
    };
    if !ctx.can_store_secrets() {
        return;
    }
    match ctx.store_secret(CERT, &seen).await {
        Ok(()) => *stored = Some(seen),
        Err(e) => tracing::warn!(error = %e, "cast certificate not stored"),
    }
}

/// Whether the speaker listens: a TCP connection opened then dropped.
async fn reachable(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect((host, port))
        )
        .await,
        Ok(Ok(_))
    )
}

fn device(ctx: &DriverCtx, id: &DeviceId, host: &str, announcements: &Announcements) -> Device {
    Device {
        id: id.clone(),
        instance: ctx.instance().clone(),
        native_name: format!("Google Cast {host}").into(),
        manufacturer: None,
        model: Some("Google Cast".into()),
        description: None,
        native_room: None,
        members: Vec::new(),
        points: announcements.points(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_speaker_needs_its_address_and_moli_s() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "host": "192.168.1.13", "media_base": "http://192.168.1.97:8790" }))
        .unwrap();
        assert_eq!(
            (
                config.port,
                config.tts.as_str(),
                config.volume,
                config.quiet_hours
            ),
            (8009, "127.0.0.1:10200", 50, false)
        );
        assert!(serde_json::from_value::<Config>(serde_json::json!({ "host": "h" })).is_err());
    }

    #[test]
    fn a_wav_says_how_long_it_plays() {
        // Piper: 22 050 Hz, mono, 16 bits = 44 100 bytes a second.
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.resize(28, 0);
        wav.extend_from_slice(&44_100u32.to_le_bytes());
        wav.resize(44 + 88_200, 0);
        assert_eq!(length(&wav), Duration::from_secs(2));
        assert_eq!(length(b"not a wav"), Duration::from_secs(60));
    }
}
