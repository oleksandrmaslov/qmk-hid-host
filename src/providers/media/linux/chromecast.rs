use mpris::{Metadata, MetadataValue, PlaybackStatus};
use oxicast::{CastClient, MediaMetadata, MediaStatus, PlayerState};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

#[derive(Clone)]
pub(super) struct Track {
    pub metadata: Metadata,
    pub status: PlaybackStatus,
    pub total_time: u16,
    pub position: u16,
}

fn from_status(status: &MediaStatus) -> Option<Track> {
    let playback = match status.player_state {
        PlayerState::Playing | PlayerState::Buffering => PlaybackStatus::Playing,
        PlayerState::Paused => PlaybackStatus::Paused,
        _ => return None,
    };
    let (title, artist) = match status.media.as_ref()?.metadata.as_ref()? {
        MediaMetadata::MusicTrack { title, artist, .. } => (title.as_deref(), artist.as_deref()),
        MediaMetadata::Generic { title, .. } => (title.as_deref(), None),
        _ => return None,
    };
    let title = title.map(str::trim).filter(|title| !title.is_empty())?;
    let artist = artist.unwrap_or_default().trim();
    Some(Track {
        metadata: Metadata::from(HashMap::from([
            ("xesam:title".into(), MetadataValue::from(title)),
            ("xesam:artist".into(), MetadataValue::from(vec![MetadataValue::from(artist)])),
        ])),
        status: playback,
        total_time: status.duration.unwrap_or_default() as u16,
        position: status.current_time as u16,
    })
}

pub(super) fn current(receiver: Option<&watch::Receiver<Option<Track>>>, local_playing: bool) -> Option<Track> {
    let receiver = receiver?;
    if receiver.has_changed().is_err() {
        return None;
    }
    let track = receiver.borrow().clone()?;
    if track.status == PlaybackStatus::Playing || !local_playing {
        Some(track)
    } else {
        None
    }
}

pub(super) fn start(host: String, generation: Arc<AtomicU64>, run_id: u64) -> watch::Receiver<Option<Track>> {
    let (sender, receiver) = watch::channel(None);
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!("Chromecast monitor could not start: {}", error);
                return;
            }
        };
        runtime.block_on(async {
            tokio::select! {
                _ = async {
                    loop {
                        if let Err(error) = monitor(&host, &sender).await {
                            tracing::debug!("Chromecast {} unavailable: {}", host, error);
                        }
                        sender.send_replace(None);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                } => {},
                _ = async {
                    while generation.load(Relaxed) == run_id {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                } => {},
            }
        });
        tracing::info!("Chromecast monitor stopped");
    });
    receiver
}

async fn monitor(host: &str, sender: &watch::Sender<Option<Track>>) -> oxicast::Result<()> {
    let client = CastClient::builder(host, 8009)
        .connect_timeout(Duration::from_secs(3))
        .request_timeout(Duration::from_secs(3))
        .auto_reconnect(false)
        .connect()
        .await?;
    tracing::info!("Chromecast connected: {}", host);
    let result = async {
        let mut transport = String::new();
        loop {
            let receiver = client.receiver_status().await?;
            let app = receiver.applications.iter().find(|app| {
                app.namespaces
                    .iter()
                    .any(|namespace| namespace == "urn:x-cast:com.google.cast.media")
            });
            let status = if let Some(app) = app {
                if transport != app.transport_id {
                    let status = client.join_session(app).await?;
                    transport = app.transport_id.clone();
                    status
                } else {
                    client.media_status().await?
                }
            } else {
                transport.clear();
                None
            };
            sender.send_replace(status.as_ref().and_then(from_status));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    .await;
    sender.send_replace(None);
    // Closing our observer must leave the receiver's playback running.
    let _ = client.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_type::DataType;
    use oxicast::{MediaInfo, MediaMetadata, PlayerState, Volume};
    use tokio::sync::broadcast;

    fn soundcloud_status() -> MediaStatus {
        MediaStatus {
            media_session_id: 1,
            player_state: PlayerState::Playing,
            idle_reason: None,
            current_time: 227.593392,
            duration: Some(384.113),
            volume: Volume::new(1.0),
            media: Some(
                MediaInfo::new("soundcloud-track", "audio/mpeg").metadata(MediaMetadata::MusicTrack {
                    title: Some("Promise".into()),
                    artist: Some("BenHowardOfficial".into()),
                    album_name: None,
                    composer: None,
                    track_number: None,
                    disc_number: None,
                    images: vec![],
                }),
            ),
        }
    }

    #[test]
    fn soundcloud_cast_track_reaches_keyboard_reports() {
        let status = soundcloud_status();
        let track = from_status(&status).expect("active Chromecast track was discarded");
        assert_eq!(track.status, PlaybackStatus::Playing);
        assert_eq!((track.total_time, track.position), (384, 227));
        let (sender, mut receiver) = broadcast::channel(16);
        let data = super::super::send_media_data(&track.metadata, &sender, None);
        assert_eq!(data, ("BenHowardOfficial".into(), "Promise".into()));
        let artist = receiver.try_recv().unwrap();
        assert_eq!(artist[0], DataType::MediaArtist as u8);
        assert_eq!(&artist[2..], b"BenHowardOfficial");
        let title = receiver.try_recv().unwrap();
        assert_eq!(title[0], DataType::MediaTitle as u8);
        assert_eq!(&title[2..], b"Promise");
        assert_eq!(receiver.try_recv().unwrap()[0], DataType::MediaPlayerLinux as u8);
    }

    #[test]
    fn cast_priority_and_disconnect_allow_local_fallback() {
        let mut status = soundcloud_status();
        let (sender, receiver) = watch::channel(from_status(&status));
        assert!(current(Some(&receiver), true).is_some());
        status.player_state = PlayerState::Paused;
        sender.send_replace(from_status(&status));
        assert!(current(Some(&receiver), true).is_none());
        assert!(current(Some(&receiver), false).is_some());
        status.player_state = PlayerState::Idle;
        sender.send_replace(from_status(&status));
        assert!(current(Some(&receiver), false).is_none());
        status.player_state = PlayerState::Playing;
        sender.send_replace(from_status(&status));
        drop(sender);
        assert!(current(Some(&receiver), false).is_none());
    }

    #[test]
    fn incomplete_cast_metadata_and_time_limits_are_handled() {
        let mut status = soundcloud_status();
        status.duration = Some(100_000.0);
        status.current_time = -1.0;
        let track = from_status(&status).unwrap();
        assert_eq!((track.total_time, track.position), (u16::MAX, 0));
        status.player_state = PlayerState::Buffering;
        assert_eq!(from_status(&status).unwrap().status, PlaybackStatus::Playing);
        let media = status.media.as_mut().unwrap();
        media.metadata = Some(MediaMetadata::Generic {
            title: Some(" Promise ".into()),
            subtitle: None,
            images: vec![],
        });
        let track = from_status(&status).unwrap();
        assert_eq!(track.metadata.title(), Some("Promise"));
        assert_eq!(super::super::get_artist(&track.metadata), "");
        if let Some(MediaMetadata::Generic { title, .. }) = status.media.as_mut().unwrap().metadata.as_mut() {
            *title = Some("  ".into());
        }
        assert!(from_status(&status).is_none());
        status.media = None;
        assert!(from_status(&status).is_none());
    }

    #[test]
    #[ignore = "requires QMK_CHROMECAST_HOST and an active Cast track; does not control playback"]
    fn live_chromecast_reports_track_and_stops_monitor() {
        let host = std::env::var("QMK_CHROMECAST_HOST").expect("set QMK_CHROMECAST_HOST to a Cast hostname or IP");
        let generation = Arc::new(AtomicU64::new(0));
        let receiver = start(host, generation.clone(), 0);
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        let track = loop {
            let track = current(Some(&receiver), false);
            if track.is_some() || std::time::Instant::now() >= deadline {
                break track;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        generation.fetch_add(1, Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while Arc::strong_count(&generation) > 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Arc::strong_count(&generation), 1, "stopped Chromecast monitor is still running");
        assert!(
            current(Some(&receiver), false).is_none(),
            "closed monitor retained stale track data"
        );
        let track = track.expect("no active Chromecast track was reported");
        let (sender, mut reports) = broadcast::channel(16);
        let (artist, title) = super::super::send_media_data(&track.metadata, &sender, None);
        println!("Chromecast track: {} — {}", title, artist);
        assert!(!title.is_empty());
        assert_eq!(
            &reports.try_recv().unwrap()[2..],
            super::super::truncate_to_bytes(&artist, 30).as_bytes()
        );
        assert_eq!(
            &reports.try_recv().unwrap()[2..],
            super::super::truncate_to_bytes(&title, 30).as_bytes()
        );
    }
}
