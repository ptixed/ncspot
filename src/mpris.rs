use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;
use zbus::zvariant::{ObjectPath, Value};
use zbus::{dbus_interface, ConnectionBuilder};

use crate::application::ASYNC_RUNTIME;
use crate::library::Library;
use crate::model::playable::Playable;
use crate::queue::RepeatSetting;
use crate::traits::ListItem;
use crate::{
    events::EventManager,
    queue::Queue,
    spotify::{PlayerEvent, Spotify, VOLUME_PERCENT},
};

struct MprisRoot {}

#[dbus_interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    #[dbus_interface(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[dbus_interface(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[dbus_interface(property)]
    fn has_tracklist(&self) -> bool {
        true
    }

    #[dbus_interface(property)]
    fn identity(&self) -> &str {
        "ncspot"
    }

    #[dbus_interface(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        vec!["spotify".to_string()]
    }

    #[dbus_interface(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }

    fn raise(&self) {}

    fn quit(&self) {}
}

struct MprisPlayer {
    event: EventManager,
    queue: Arc<Queue>,
    library: Arc<Library>,
    spotify: Spotify,
}

#[dbus_interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    #[dbus_interface(property)]
    fn playback_status(&self) -> &str {
        match self.spotify.get_current_status() {
            PlayerEvent::Playing(_) | PlayerEvent::FinishedTrack => "Playing",
            PlayerEvent::Paused(_) => "Paused",
            _ => "Stopped",
        }
    }

    #[dbus_interface(property)]
    fn loop_status(&self) -> &str {
        match self.queue.get_repeat() {
            RepeatSetting::None => "None",
            RepeatSetting::RepeatTrack => "Track",
            RepeatSetting::RepeatPlaylist => "Playlist",
        }
    }

    #[dbus_interface(property)]
    fn set_loop_status(&self, loop_status: &str) {
        let setting = match loop_status {
            "Track" => RepeatSetting::RepeatTrack,
            "Playlist" => RepeatSetting::RepeatPlaylist,
            _ => RepeatSetting::None,
        };
        self.queue.set_repeat(setting);
        self.event.trigger();
    }

    #[dbus_interface(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[dbus_interface(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[dbus_interface(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[dbus_interface(property)]
    fn metadata(&self) -> HashMap<String, Value> {
        let mut hm = HashMap::new();

        let playable = self.queue.get_current();

        // Fetch full track details in case this playable is based on a SimplifiedTrack
        // This is necessary because SimplifiedTrack objects don't contain a cover_url
        let playable_full = playable.and_then(|p| match p {
            Playable::Track(track) => {
                if track.cover_url.is_some() {
                    // We already have `cover_url`, no need to fetch the full track
                    Some(Playable::Track(track))
                } else {
                    self.spotify
                        .api
                        .track(&track.id.unwrap_or_default())
                        .as_ref()
                        .map(|t| Playable::Track(t.into()))
                }
            }
            Playable::Episode(episode) => Some(Playable::Episode(episode)),
        });
        let playable = playable_full.as_ref();

        hm.insert(
            "mpris:trackid".to_string(),
            Value::ObjectPath(ObjectPath::from_string_unchecked(format!(
                "/org/ncspot/{}",
                playable
                    .filter(|t| t.id().is_some())
                    .map(|t| t.uri().replace(':', "/"))
                    .unwrap_or_else(|| String::from("0"))
            ))),
        );
        hm.insert(
            "mpris:length".to_string(),
            Value::I64(playable.map(|t| t.duration() as i64 * 1_000).unwrap_or(0)),
        );
        hm.insert(
            "mpris:artUrl".to_string(),
            Value::Str(
                playable
                    .map(|t| t.cover_url().unwrap_or_default())
                    .unwrap_or_default()
                    .into(),
            ),
        );

        hm.insert(
            "xesam:album".to_string(),
            Value::Str(
                playable
                    .and_then(|p| p.track())
                    .map(|t| t.album.unwrap_or_default())
                    .unwrap_or_default()
                    .into(),
            ),
        );
        hm.insert(
            "xesam:albumArtist".to_string(),
            Value::Array(
                playable
                    .and_then(|p| p.track())
                    .map(|t| t.album_artists)
                    .unwrap_or_default()
                    .into(),
            ),
        );
        hm.insert(
            "xesam:artist".to_string(),
            Value::Array(
                playable
                    .and_then(|p| p.track())
                    .map(|t| t.artists)
                    .unwrap_or_default()
                    .into(),
            ),
        );
        hm.insert(
            "xesam:discNumber".to_string(),
            Value::I32(
                playable
                    .and_then(|p| p.track())
                    .map(|t| t.disc_number)
                    .unwrap_or(0),
            ),
        );
        hm.insert(
            "xesam:title".to_string(),
            Value::Str(
                playable
                    .map(|t| match t {
                        Playable::Track(t) => t.title.clone(),
                        Playable::Episode(ep) => ep.name.clone(),
                    })
                    .unwrap_or_default()
                    .into(),
            ),
        );
        hm.insert(
            "xesam:trackNumber".to_string(),
            Value::I32(
                playable
                    .and_then(|p| p.track())
                    .map(|t| t.track_number)
                    .unwrap_or(0) as i32,
            ),
        );
        hm.insert(
            "xesam:url".to_string(),
            Value::Str(
                playable
                    .map(|t| t.share_url().unwrap_or_default())
                    .unwrap_or_default()
                    .into(),
            ),
        );
        hm.insert(
            "xesam:userRating".to_string(),
            Value::F64(
                playable
                    .and_then(|p| p.track())
                    .map(|t| match self.library.is_saved_track(&Playable::Track(t)) {
                        true => 1.0,
                        false => 0.0,
                    })
                    .unwrap_or(0.0),
            ),
        );

        hm
    }

    #[dbus_interface(property)]
    fn shuffle(&self) -> bool {
        self.queue.get_shuffle()
    }

    #[dbus_interface(property)]
    fn set_shuffle(&self, shuffle: bool) {
        self.queue.set_shuffle(shuffle);
        self.event.trigger();
    }

    #[dbus_interface(property)]
    fn volume(&self) -> f64 {
        self.spotify.volume() as f64 / 65535_f64
    }

    #[dbus_interface(property)]
    fn set_volume(&self, volume: f64) {
        log::info!("set volume: {volume}");
        if (0.0..=1.0).contains(&volume) {
            let vol = (VOLUME_PERCENT as f64) * volume * 100.0;
            self.spotify.set_volume(vol as u16);
            self.event.trigger();
        }
    }

    #[dbus_interface(property)]
    fn position(&self) -> i64 {
        self.spotify.get_current_progress().as_micros() as i64
    }

    #[dbus_interface(property)]
    fn can_go_next(&self) -> bool {
        self.queue.next_index().is_some()
    }

    #[dbus_interface(property)]
    fn can_go_previous(&self) -> bool {
        self.queue.previous_index().is_some()
    }

    #[dbus_interface(property)]
    fn can_play(&self) -> bool {
        self.queue.get_current().is_some()
    }

    #[dbus_interface(property)]
    fn can_pause(&self) -> bool {
        self.queue.get_current().is_some()
    }

    #[dbus_interface(property)]
    fn can_seek(&self) -> bool {
        self.queue.get_current().is_some()
    }

    #[dbus_interface(property)]
    fn can_control(&self) -> bool {
        self.queue.get_current().is_some()
    }

    fn next(&self) {
        self.queue.next(true)
    }

    fn previous(&self) {
        if self.spotify.get_current_progress() < Duration::from_secs(5) {
            self.queue.previous();
        } else {
            self.spotify.seek(0);
        }
    }

    fn pause(&self) {
        self.spotify.pause()
    }

    fn play_pause(&self) {
        self.queue.toggleplayback()
    }

    fn stop(&self) {
        self.queue.stop()
    }

    fn play(&self) {
        self.spotify.play()
    }

    fn seek(&self, offset: i64) {
        if let Some(current_track) = self.queue.get_current() {
            let progress = self.spotify.get_current_progress();
            let new_position = (progress.as_secs() * 1000) as i32
                + progress.subsec_millis() as i32
                + (offset / 1000) as i32;
            let new_position = new_position.max(0) as u32;
            let duration = current_track.duration();

            if new_position < duration {
                self.spotify.seek(new_position);
            } else {
                self.queue.next(true);
            }
        }
    }

    fn set_position(&self, _track: ObjectPath, position: i64) {
        if let Some(current_track) = self.queue.get_current() {
            let position = (position / 1000) as u32;
            let duration = current_track.duration();

            if position < duration {
                self.spotify.seek(position);
            }
        }
    }

    fn open_uri(&self, uri: &str) {
        self.queue.open_uri(uri);
    }
}

pub struct MprisManager {
    tx: mpsc::UnboundedSender<()>,
}

impl MprisManager {
    pub fn new(
        event: EventManager,
        queue: Arc<Queue>,
        library: Arc<Library>,
        spotify: Spotify,
    ) -> Self {
        let root = MprisRoot {};
        let player = MprisPlayer {
            event,
            queue,
            library,
            spotify,
        };

        let (tx, rx) = mpsc::unbounded_channel::<()>();

        ASYNC_RUNTIME.get().unwrap().spawn(async {
            let result = Self::serve(UnboundedReceiverStream::new(rx), root, player).await;
            if let Err(e) = result {
                log::error!("MPRIS error: {e}");
            }
        });

        Self { tx }
    }

    async fn serve(
        mut rx: UnboundedReceiverStream<()>,
        root: MprisRoot,
        player: MprisPlayer,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let conn = ConnectionBuilder::session()?
            .name(instance_bus_name())?
            .serve_at("/org/mpris/MediaPlayer2", root)?
            .serve_at("/org/mpris/MediaPlayer2", player)?
            .build()
            .await?;

        let object_server = conn.object_server();
        let player_iface_ref = object_server
            .interface::<_, MprisPlayer>("/org/mpris/MediaPlayer2")
            .await?;
        let player_iface = player_iface_ref.get().await;

        loop {
            rx.next().await;
            let ctx = player_iface_ref.signal_context();
            player_iface.playback_status_changed(ctx).await?;
            player_iface.metadata_changed(ctx).await?;
        }
    }

    pub fn update(&self) {
        if let Err(e) = self.tx.send(()) {
            log::warn!("Could not update MPRIS state: {e}");
        }
    }
}

/// Get the D-Bus bus name for this instance according to the MPRIS specification.
///
/// https://specifications.freedesktop.org/mpris-spec/2.2/#Bus-Name-Policy
pub fn instance_bus_name() -> String {
    format!(
        "org.mpris.MediaPlayer2.ncspot.instance{}",
        std::process::id()
    )
}
