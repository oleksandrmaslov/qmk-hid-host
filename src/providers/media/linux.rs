use mpris::{Metadata, PlayerFinder};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::data_type::DataType;

use super::super::_base::Provider;

fn find_media_player(finder: &PlayerFinder) -> Result<mpris::Player, mpris::FindingError> {
    let mut players: Vec<_> = finder.iter_players()?.filter_map(Result::ok).collect();
    let browser_pid = players
        .iter()
        .find(|player| player.bus_name() == "org.mpris.MediaPlayer2.plasma-browser-integration")
        .and_then(|player| player.get_metadata().ok())
        .filter(|metadata| metadata.title().map(str::trim).is_some_and(|title| !title.is_empty()))
        .and_then(|metadata| metadata.get("kde:pid").and_then(|value| value.as_i64()))
        .filter(|pid| *pid > 0);
    if let Some(pid) = browser_pid {
        // KDE's tagged source replaces only the native source for that browser process.
        let duplicate = format!("org.mpris.MediaPlayer2.chromium.instance{pid}");
        players.retain(|player| player.bus_name() != duplicate);
    }
    players
        .into_iter()
        .min_by_key(|player| {
            let status = match player.get_playback_status() {
                Ok(mpris::PlaybackStatus::Playing) => 0,
                Ok(mpris::PlaybackStatus::Paused) => 1,
                Ok(mpris::PlaybackStatus::Stopped) => 2,
                Err(_) => 3,
            };
            let missing_artist = player
                .get_metadata()
                .map(|metadata| get_artist(&metadata).is_empty())
                .unwrap_or(true);
            (status, missing_artist)
        })
        .ok_or(mpris::FindingError::NoPlayerFound)
}

fn get_artist(metadata: &Metadata) -> &str {
    metadata
        .artists()
        .and_then(|artists| artists.into_iter().map(str::trim).find(|artist| !artist.is_empty()))
        .unwrap_or_default()
}

fn send_media_data(
    metadata: &Metadata,
    data_sender: &broadcast::Sender<Vec<u8>>,
    current: &(String, String),
    fallback_title: &str,
) -> (String, String) {
    let (mut artist, mut title) = current.clone();

    let new_artist = get_artist(metadata).to_string();
    if artist != new_artist {
        tracing::info!("new artist: {}", new_artist);
        artist = new_artist;
        send_media_raw(DataType::MediaArtist, &artist, &data_sender);
    }

    let new_title = get_display_title(metadata, fallback_title);
    if !new_title.is_empty() && title != new_title {
        tracing::info!("new title: {}", new_title);
        title = new_title;
        send_media_raw(DataType::MediaTitle, &title, &data_sender);
        send_media_player_text(&title, &data_sender);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    return (artist, title);
}

fn get_display_title(metadata: &Metadata, fallback_title: &str) -> String {
    if let Some(title) = metadata.title().map(str::trim).filter(|title| !title.is_empty()) {
        return title.to_string();
    }

    if let Some(url) = metadata.url().map(str::trim).filter(|url| !url.is_empty()) {
        let path = url.strip_prefix("file://").unwrap_or(url);
        if let Some(file_name) = Path::new(path).file_name().and_then(|name| name.to_str()) {
            return file_name.to_string();
        }
        return url.to_string();
    }

    let fallback_title = fallback_title.trim();
    if !fallback_title.is_empty() {
        tracing::info!("media metadata has no title or url, using player identity: {}", fallback_title);
        return fallback_title.to_string();
    }

    tracing::info!("media metadata has no title, url, or player identity: {:?}", metadata);
    String::default()
}

fn send_media_raw(data_type: DataType, value: &str, data_sender: &broadcast::Sender<Vec<u8>>) {
    let mut data = truncate_to_bytes(value, 30).as_bytes().to_vec();
    data.insert(0, data.len() as u8);
    data.insert(0, data_type as u8);
    if let Err(e) = data_sender.send(data) {
        tracing::error!("Media Provider failed to send data: {:?}", e);
    }
}

fn send_media_player_text(value: &str, data_sender: &broadcast::Sender<Vec<u8>>) {
    let mut payload = compact_media_text(value).into_bytes();
    payload.resize(8, b' ');

    let mut data = vec![DataType::MediaPlayerLinux as u8];
    data.extend_from_slice(&payload);

    if let Err(e) = data_sender.send(data) {
        tracing::error!("Media Provider failed to send compact media data: {:?}", e);
    }
}

fn compact_media_text(value: &str) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(6).collect();

    if chars.next().is_some() {
        truncate_utf8_bytes(&format!("{}..", prefix), 8)
    } else {
        truncate_utf8_bytes(&value, 8)
    }
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }

    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);

    value[..end].to_string()
}

fn truncate_to_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &value[..boundary]
}

fn send_media_extended(data_sender: &broadcast::Sender<Vec<u8>>, total_time: u16, position: u16, status: u8, artist: &str) {
    let artist = truncate_to_bytes(artist, 21);

    let mut data = vec![DataType::MediaExtended as u8];
    data.extend_from_slice(&total_time.to_le_bytes());
    data.extend_from_slice(&position.to_le_bytes());
    data.push(status);
    data.push(artist.len() as u8);
    data.extend_from_slice(artist.as_bytes());

    if let Err(e) = data_sender.send(data) {
        tracing::error!("Media Provider failed to send extended media data: {:?}", e);
    }
}

pub struct MediaProvider {
    data_sender: broadcast::Sender<Vec<u8>>,
    generation: Arc<AtomicU64>,
    extended: bool,
}

impl MediaProvider {
    pub fn new(data_sender: broadcast::Sender<Vec<u8>>, extended: bool) -> Box<dyn Provider> {
        let provider = MediaProvider {
            data_sender,
            generation: Arc::new(AtomicU64::new(0)),
            extended,
        };
        return Box::new(provider);
    }
}

impl Provider for MediaProvider {
    fn start(&self) {
        tracing::info!("Media Provider started");
        let run_id = self.generation.fetch_add(1, Relaxed) + 1;
        let data_sender = self.data_sender.clone();
        let generation = self.generation.clone();
        let extended = self.extended;
        std::thread::spawn(move || {
            let mut media_data = (String::default(), String::default());

            'outer: loop {
                if generation.load(Relaxed) != run_id {
                    break;
                }

                if let Ok(Ok(player)) = PlayerFinder::new().map(|x| find_media_player(&x)) {
                    let metadata = player.get_metadata().ok();
                    if let Some(ref metadata) = metadata {
                        if generation.load(Relaxed) != run_id {
                            break;
                        }
                        media_data = send_media_data(metadata, &data_sender, &media_data, player.identity());
                        if extended {
                            let status = player.get_playback_status().ok().map(status_to_byte).unwrap_or(0);
                            let (total_time, position) = get_time_info(&player, metadata);
                            send_media_extended(&data_sender, total_time, position, status, &media_data.0);
                        }
                    }

                    if let Ok(mut tracker) = player.track_progress(100) {
                        loop {
                            let tick = tracker.tick();
                            if generation.load(Relaxed) != run_id {
                                break 'outer;
                            }
                            if tick.progress_changed {
                                let metadata = tick.progress.metadata();
                                media_data = send_media_data(metadata, &data_sender, &media_data, player.identity());
                                if extended {
                                    let (total_time, position) = get_time_info(&player, metadata);
                                    let status = status_to_byte(tick.progress.playback_status());
                                    send_media_extended(&data_sender, total_time, position, status, &media_data.0);
                                }
                            }
                            if tick.player_quit || tick.progress.playback_status() != mpris::PlaybackStatus::Playing {
                                break;
                            }
                        }
                    }
                }

                tracing::info!("waiting for player...");

                for _ in 0..10 {
                    if generation.load(Relaxed) != run_id {
                        break 'outer;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }

            tracing::info!("Media Provider stopped");
        });
    }

    fn stop(&self) {
        self.generation.fetch_add(1, Relaxed);
    }
}

fn get_time_info(player: &mpris::Player, metadata: &mpris::Metadata) -> (u16, u16) {
    let total_time = metadata
        .length_in_microseconds()
        .map(|us| (us / 1_000_000).min(65535) as u16)
        .unwrap_or(0);
    let position = player
        .get_position_in_microseconds()
        .ok()
        .map(|us| (us / 1_000_000).min(65535) as u16)
        .unwrap_or(0);
    (total_time, position)
}

fn status_to_byte(status: mpris::PlaybackStatus) -> u8 {
    match status {
        mpris::PlaybackStatus::Playing => 1,
        mpris::PlaybackStatus::Paused => 2,
        mpris::PlaybackStatus::Stopped => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpris::MetadataValue;
    use std::collections::HashMap;

    fn metadata(artists: &[&str], title: &str) -> Metadata {
        Metadata::from(HashMap::from([
            (
                "xesam:artist".into(),
                MetadataValue::from(artists.iter().map(|artist| MetadataValue::from(*artist)).collect::<Vec<_>>()),
            ),
            ("xesam:title".into(), MetadataValue::from(title)),
        ]))
    }

    #[test]
    fn missing_artist_clears_previous_artist_report() {
        let (sender, mut receiver) = broadcast::channel(16);
        let before = ("Nico".to_string(), "Unfold".to_string());
        let after = send_media_data(&metadata(&[""], "Unfold"), &sender, &before, "Chrome");
        assert_eq!(after, (String::new(), "Unfold".to_string()));
        assert_eq!(receiver.try_recv().unwrap(), vec![DataType::MediaArtist as u8, 0]);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn artist_update_does_not_require_a_title_change() {
        let (sender, mut receiver) = broadcast::channel(16);
        let before = ("Previous artist".to_string(), "Unfold".to_string());
        let after = send_media_data(&metadata(&["", " Nico "], "Unfold"), &sender, &before, "Chrome");
        assert_eq!(after, ("Nico".to_string(), "Unfold".to_string()));
        assert_eq!(
            receiver.try_recv().unwrap(),
            vec![DataType::MediaArtist as u8, 4, b'N', b'i', b'c', b'o']
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn media_reports_keep_utf8_valid_at_the_report_boundary() {
        let (sender, mut receiver) = broadcast::channel(16);
        send_media_raw(DataType::MediaArtist, &format!("{}é", "a".repeat(29)), &sender);
        let report = receiver.try_recv().unwrap();
        assert_eq!(report[1] as usize, report.len() - 2);
        assert!(report.len() <= 32);
        assert!(std::str::from_utf8(&report[2..]).is_ok());
    }

    #[test]
    #[ignore = "requires Chrome and KDE browser integration for the same process, with an artist tag"]
    fn active_browser_media_uses_the_source_with_an_artist() {
        let finder = PlayerFinder::new().unwrap();
        let integration = finder
            .iter_players()
            .unwrap()
            .filter_map(Result::ok)
            .find(|player| player.bus_name() == "org.mpris.MediaPlayer2.plasma-browser-integration")
            .unwrap();
        let native = finder
            .iter_players()
            .unwrap()
            .filter_map(Result::ok)
            .find(|player| player.bus_name().starts_with("org.mpris.MediaPlayer2.chromium."))
            .unwrap();
        let metadata = integration.get_metadata().unwrap();
        let pid = metadata.get("kde:pid").and_then(|value| value.as_i64()).unwrap();
        assert_eq!(native.bus_name(), format!("org.mpris.MediaPlayer2.chromium.instance{pid}"));
        assert!(integration
            .get_metadata()
            .unwrap()
            .artists()
            .unwrap()
            .iter()
            .any(|name| !name.trim().is_empty()));
        let selected = find_media_player(&finder).unwrap();
        assert_eq!(selected.bus_name(), integration.bus_name());
        let expected = integration.get_metadata().unwrap();
        let actual = selected.get_metadata().unwrap();
        assert_eq!(actual.artists(), expected.artists());
        assert_eq!(actual.title(), expected.title());
    }

    #[test]
    #[ignore = "requires a stable active media player; does not control playback"]
    fn media_worker_stops_without_waiting_for_the_next_track() {
        let (sender, mut receiver) = broadcast::channel(16);
        let running = Arc::new(AtomicU64::new(0));
        let provider = MediaProvider {
            data_sender: sender,
            generation: running.clone(),
            extended: false,
        };
        provider.start();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while receiver.try_recv().is_err() {
            assert!(std::time::Instant::now() < deadline, "media provider did not start");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        provider.stop();
        provider.start();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while Arc::strong_count(&running) > 3 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            Arc::strong_count(&running),
            3,
            "previous media worker survived an immediate restart"
        );
        provider.stop();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while Arc::strong_count(&running) > 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(Arc::strong_count(&running), 2, "stopped media worker is still holding its state");
    }
}
