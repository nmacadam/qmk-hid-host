use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use tokio::sync::broadcast;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession, GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};

use crate::data_type::DataType;

use super::super::_base::Provider;

// Media info is polled once a second instead of waiting for Windows' change
// events: those often fire before the new song's properties are readable (so the
// change is missed), and they follow only the "current" session, which is often
// not the one playing (a paused browser tab, say).

fn get_manager() -> Result<GlobalSystemMediaTransportControlsSessionManager, ()> {
    return GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .and_then(|manager| manager.get())
        .map_err(|e| tracing::error!("Can not get Session Manager: {}", e));
}

fn status_of(session: &GlobalSystemMediaTransportControlsSession) -> u8 {
    match session.GetPlaybackInfo().and_then(|info| info.PlaybackStatus()) {
        Ok(s) if s == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing => 2,
        Ok(s) if s == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Paused => 1,
        _ => 0,
    }
}

/// The session to show: the first one that is playing, else the current one.
fn pick_session(manager: &GlobalSystemMediaTransportControlsSessionManager) -> Option<GlobalSystemMediaTransportControlsSession> {
    if let Ok(sessions) = manager.GetSessions() {
        let n = sessions.Size().unwrap_or(0);
        for i in 0..n {
            if let Ok(session) = sessions.GetAt(i) {
                if status_of(&session) == 2 {
                    return Some(session);
                }
            }
        }
    }
    manager.GetCurrentSession().ok()
}

fn get_media_data(session: &GlobalSystemMediaTransportControlsSession) -> Option<(String, String)> {
    if let Ok(media_properties) = session.TryGetMediaPropertiesAsync().and_then(|x| x.get()) {
        let artist = media_properties.Artist().unwrap_or_default().to_string();
        let title = media_properties.Title().unwrap_or_default().to_string();

        if !artist.is_empty() || !title.is_empty() {
            return Some((artist, title));
        }
    }

    None
}

fn send_text(data_type: DataType, value: &String, data_sender: &broadcast::Sender<Vec<u8>>) {
    let mut data = value.to_string().into_bytes();
    data.truncate(30);
    data.insert(0, data.len() as u8);
    data.insert(0, data_type as u8);
    if let Err(e) = data_sender.send(data) {
        tracing::error!("Can not send data: {}", e);
    }

    // short delay to separate title/artist data
    std::thread::sleep(std::time::Duration::from_millis(10));
}

/// [0xB2, state]: 0 = nothing playing (no session, stopped, closed), 1 = paused, 2 = playing.
fn send_state(state: u8, data_sender: &broadcast::Sender<Vec<u8>>) {
    if let Err(e) = data_sender.send(vec![DataType::MediaState as u8, state]) {
        tracing::error!("Can not send data: {}", e);
    }
}

pub struct MediaProvider {
    data_sender: broadcast::Sender<Vec<u8>>,
    is_started: Arc<AtomicBool>,
}

impl MediaProvider {
    pub fn new(data_sender: broadcast::Sender<Vec<u8>>) -> Box<dyn Provider> {
        let provider = MediaProvider {
            data_sender,
            is_started: Arc::new(AtomicBool::new(false)),
        };
        return Box::new(provider);
    }
}

impl Provider for MediaProvider {
    fn start(&self) {
        tracing::info!("Media Provider started");
        self.is_started.store(true, Relaxed);
        let data_sender = self.data_sender.clone();
        let is_started = self.is_started.clone();
        std::thread::spawn(move || {
            let manager = match get_manager() {
                Ok(m) => m,
                Err(_) => return,
            };

            // Everything is sent once at start (so a newly connected keyboard
            // gets it), then only when it changes.
            let mut synced_artist: Option<String> = None;
            let mut synced_title: Option<String> = None;
            let mut synced_state: Option<u8> = None;
            let mut ticks = 0u32;
            loop {
                if !is_started.load(Relaxed) {
                    break;
                }

                if ticks % 10 == 0 {
                    let session = pick_session(&manager);
                    let state = session.as_ref().map(status_of).unwrap_or(0);
                    if let Some((artist, title)) = session.as_ref().and_then(get_media_data) {
                        if synced_artist.as_ref() != Some(&artist) {
                            send_text(DataType::MediaArtist, &artist, &data_sender);
                            synced_artist = Some(artist);
                        }
                        if synced_title.as_ref() != Some(&title) {
                            send_text(DataType::MediaTitle, &title, &data_sender);
                            synced_title = Some(title);
                        }
                    }
                    if synced_state != Some(state) {
                        send_state(state, &data_sender);
                        synced_state = Some(state);
                    }
                }
                ticks = ticks.wrapping_add(1);

                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            tracing::info!("Media Provider stopped");
        });
    }

    fn stop(&self) {
        self.is_started.store(false, Relaxed);
    }
}
