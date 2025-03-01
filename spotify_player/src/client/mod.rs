use std::{borrow::Cow, sync::Arc};

#[cfg(feature = "streaming")]
use crate::streaming;
use crate::{
    config,
    event::{ClientRequest, PlayerRequest},
    state::*,
};

#[cfg(any(feature = "lyric-finder", feature = "image"))]
use anyhow::Context as _;
use anyhow::Result;
use librespot_core::session::Session;
use rspotify::prelude::*;

mod handlers;
mod spotify;

pub use handlers::*;

/// The application's client
#[derive(Clone)]
pub struct Client {
    spotify: Arc<spotify::Spotify>,
    http: reqwest::Client,
}

impl Client {
    /// creates a new client
    pub fn new(session: Session, device: config::DeviceConfig, client_id: String) -> Self {
        Self {
            spotify: Arc::new(spotify::Spotify::new(session, device, client_id)),
            http: reqwest::Client::new(),
        }
    }

    /// creates a new streaming connection
    #[cfg(feature = "streaming")]
    pub async fn new_streaming_connection(
        &self,
        streaming_sub: flume::Receiver<()>,
        client_pub: flume::Sender<ClientRequest>,
    ) -> Result<String> {
        let session = match self.spotify.session {
            None => {
                anyhow::bail!("No Spotify session found.");
            }
            Some(ref session) => session.clone(),
        };
        let device = self.spotify.device.clone();
        let device_id = session.device_id().to_string();
        streaming::new_connection(session, device, client_pub, streaming_sub)?;

        Ok(device_id)
    }

    /// initializes the authentication token inside the Spotify client
    pub async fn init_token(&self) -> Result<()> {
        self.spotify.refresh_token().await?;
        Ok(())
    }

    /// handles a player request
    async fn handle_player_request(
        &self,
        state: &SharedState,
        request: PlayerRequest,
    ) -> Result<()> {
        // `TransferPlayback` needs to be handled separately
        // from other play requests because they don't require an active playback
        // transfer the current playback to another device
        if let PlayerRequest::TransferPlayback(device_id, force_play) = request {
            self.spotify
                .transfer_playback(&device_id, Some(force_play))
                .await?;

            tracing::info!("Transfered the playback to device with {} id", device_id);
            return Ok(());
        }

        let playback = match state.player.read().simplified_playback() {
            Some(playback) => playback,
            None => {
                anyhow::bail!("failed to handle the player request: there is no active playback");
            }
        };
        let device_id = playback.device_id.as_deref();

        match request {
            PlayerRequest::NextTrack => self.spotify.next_track(device_id).await?,
            PlayerRequest::PreviousTrack => self.spotify.previous_track(device_id).await?,
            PlayerRequest::ResumePause => {
                if !playback.is_playing {
                    self.spotify.resume_playback(device_id, None).await?
                } else {
                    self.spotify.pause_playback(device_id).await?
                }
            }
            PlayerRequest::SeekTrack(position_ms) => {
                self.spotify.seek_track(position_ms, device_id).await?
            }
            PlayerRequest::Repeat => {
                let next_repeat_state = match playback.repeat_state {
                    rspotify_model::RepeatState::Off => rspotify_model::RepeatState::Track,
                    rspotify_model::RepeatState::Track => rspotify_model::RepeatState::Context,
                    rspotify_model::RepeatState::Context => rspotify_model::RepeatState::Off,
                };

                self.spotify.repeat(&next_repeat_state, device_id).await?
            }
            PlayerRequest::Shuffle => {
                self.spotify
                    .shuffle(!playback.shuffle_state, device_id)
                    .await?
            }
            PlayerRequest::Volume(volume) => self.spotify.volume(volume, device_id).await?,
            PlayerRequest::StartPlayback(p) => {
                self.start_playback(p, device_id).await?;
                // for some reasons, when starting a new playback, the integrated `spotify-player`
                // client doesn't respect the initial shuffle state, so we need to manually update the state
                self.spotify
                    .shuffle(playback.shuffle_state, device_id)
                    .await?
            }
            PlayerRequest::TransferPlayback(..) => {
                anyhow::bail!("`TransferPlayback` should be handled ealier")
            }
        };

        Ok(())
    }

    /// handles a client request
    pub async fn handle_request(&self, state: &SharedState, request: ClientRequest) -> Result<()> {
        let timer = std::time::SystemTime::now();

        match request {
            ClientRequest::ConnectDevice(id) => {
                // Device connection can fail when the specified device hasn't shown up
                // in the Spotify's server, which makes the `TransferPlayback` request fail
                // with an error like "404 Not Found".
                // This is why we need a retry mechanism to make multiple connect requests.
                let delay = std::time::Duration::from_secs(1);

                for _ in 0..10 {
                    tokio::time::sleep(delay).await;

                    let id = match &id {
                        Some(id) => Some(Cow::Borrowed(id)),
                        None => {
                            // no device id is specified, try to connect to an available device
                            match self
                                .find_available_device(&state.app_config.default_device)
                                .await
                            {
                                Ok(Some(id)) => Some(Cow::Owned(id)),
                                Ok(None) => {
                                    tracing::info!("No device found.");
                                    None
                                }
                                Err(err) => {
                                    tracing::error!("Failed to find an available device: {err}");
                                    None
                                }
                            }
                        }
                    };

                    if let Some(id) = id {
                        tracing::info!("Trying to connect to device (id={id})");
                        if let Err(err) = self.spotify.transfer_playback(&id, Some(false)).await {
                            tracing::warn!("Connection failed (device_id={id}): {err}");
                        } else {
                            tracing::info!("Connection succeeded (device_id={id})!");
                            self.update_playback(state);
                            break;
                        }
                    }
                }
            }
            ClientRequest::GetBrowseCategories => {
                let categories = self.browse_categories().await?;
                state.data.write().browse.categories = categories;
            }
            ClientRequest::GetBrowseCategoryPlaylists(category) => {
                let playlists = self.browse_category_playlists(&category.id).await?;
                state
                    .data
                    .write()
                    .browse
                    .category_playlists
                    .insert(category.id, playlists);
            }
            #[cfg(feature = "lyric-finder")]
            ClientRequest::GetLyric { track, artists } => {
                let client = lyric_finder::Client::from_http_client(&self.http);
                let query = format!("{} {}", track, artists);

                if !state.data.read().caches.lyrics.contains(&query) {
                    let result = client.get_lyric(&query).await.context(format!(
                        "failed to get lyric for track {} - artists {}",
                        track, artists
                    ))?;

                    state.data.write().caches.lyrics.put(query, result);
                }
            }
            #[cfg(feature = "streaming")]
            ClientRequest::NewStreamingConnection => {
                anyhow::bail!("request should be already handled by the caller function");
            }
            ClientRequest::GetCurrentUser => {
                let user = self.spotify.current_user().await?;
                state.data.write().user_data.user = Some(user);
            }
            ClientRequest::Player(request) => {
                self.handle_player_request(state, request).await?;
                self.update_playback(state);
            }
            ClientRequest::GetCurrentPlayback => {
                self.update_current_playback_state(state).await?;
                #[cfg(feature = "image")]
                self.get_current_track_cover_image(state).await?;
            }
            ClientRequest::GetDevices => {
                let devices = self.spotify.device().await?;
                state.player.write().devices = devices
                    .into_iter()
                    .filter_map(Device::try_from_device)
                    .collect();
            }
            ClientRequest::GetUserPlaylists => {
                let playlists = self.current_user_playlists().await?;
                state.data.write().user_data.playlists = playlists;
            }
            ClientRequest::GetUserFollowedArtists => {
                let artists = self.current_user_followed_artists().await?;
                state.data.write().user_data.followed_artists = artists;
            }
            ClientRequest::GetUserSavedAlbums => {
                let albums = self.current_user_saved_albums().await?;
                state.data.write().user_data.saved_albums = albums;
            }
            ClientRequest::GetUserTopTracks => {
                let id = "top-tracks";
                if !state.data.read().caches.tracks.contains(id) {
                    let tracks = self.current_user_top_tracks().await?;
                    state.data.write().caches.tracks.put(id.to_string(), tracks);
                }
            }
            ClientRequest::GetUserSavedTracks => {
                let tracks = self.current_user_saved_tracks().await?;
                state.data.write().user_data.saved_tracks = tracks;
            }
            ClientRequest::GetUserRecentlyPlayedTracks => {
                let id = "recently-played-tracks";
                if !state.data.read().caches.tracks.contains(id) {
                    let tracks = self.current_user_recently_played_tracks().await?;
                    state.data.write().caches.tracks.put(id.to_string(), tracks);
                }
            }
            ClientRequest::GetContext(context) => {
                let uri = context.uri();
                if !state.data.read().caches.context.contains(&uri) {
                    let context = match context {
                        ContextId::Playlist(playlist_id) => {
                            self.playlist_context(&playlist_id).await?
                        }
                        ContextId::Album(album_id) => self.album_context(&album_id).await?,
                        ContextId::Artist(artist_id) => self.artist_context(&artist_id).await?,
                    };

                    state.data.write().caches.context.put(uri, context);
                }
            }
            ClientRequest::Search(query) => {
                if !state.data.read().caches.search.contains(&query) {
                    let results = self.search(&query).await?;

                    state.data.write().caches.search.put(query, results);
                }
            }
            ClientRequest::GetRecommendations(seed) => {
                let id = format!("recommendations::{}", seed.uri());
                if !state.data.read().caches.tracks.contains(&id) {
                    let tracks = self.recommendations(&seed).await?;

                    state.data.write().caches.tracks.put(id, tracks);
                }
            }
            ClientRequest::AddTrackToQueue(track_id) => {
                self.add_track_to_queue(&track_id).await?;
            }
            ClientRequest::AddTrackToPlaylist(playlist_id, track_id) => {
                self.add_track_to_playlist(state, &playlist_id, &track_id)
                    .await?;
            }
            ClientRequest::DeleteTrackFromPlaylist(playlist_id, track_id) => {
                self.delete_track_from_playlist(state, &playlist_id, &track_id)
                    .await?;
            }
            ClientRequest::AddToLibrary(item) => {
                self.add_to_library(state, item).await?;
            }
            ClientRequest::DeleteFromLibrary(id) => {
                self.delete_from_library(state, id).await?;
            }
        };

        tracing::info!(
            "successfully handled the client request, took: {}ms",
            timer.elapsed().unwrap().as_millis()
        );

        Ok(())
    }

    fn update_playback(&self, state: &SharedState) {
        // After handling a request that updates the player's playback,
        // update the playback state by making additional refresh requests.
        //
        // # Why needs more than one request to update the playback?
        // It may take a while for Spotify to update the new change,
        // making additional requests can help ensure that
        // the playback state is always in sync with the latest change.
        let client = self.clone();
        let state = state.clone();
        tokio::task::spawn(async move {
            let delay = std::time::Duration::from_secs(1);
            for _ in 0..5 {
                tokio::time::sleep(delay).await;
                if let Err(err) = client.update_current_playback_state(&state).await {
                    tracing::error!("Failed to refresh the player's playback: {err:#}");
                }
                #[cfg(feature = "image")]
                if let Err(err) = client.get_current_track_cover_image(&state).await {
                    tracing::error!("Failed to get the current track's cover image: {err:#}");
                }
            }
        });
    }

    /// Get Spotify's available browse categories
    pub async fn browse_categories(&self) -> Result<Vec<Category>> {
        let first_page = self
            .spotify
            .categories_manual(Some("EN"), None, Some(50), None)
            .await?;

        Ok(first_page.items.into_iter().map(Category::from).collect())
    }

    /// Get Spotify's available browse playlists of a given category
    pub async fn browse_category_playlists(&self, category_id: &str) -> Result<Vec<Playlist>> {
        let first_page = self
            .spotify
            .category_playlists_manual(category_id, None, Some(50), None)
            .await?;

        Ok(first_page.items.into_iter().map(Playlist::from).collect())
    }

    /// Find an available device. If found, return the device ID.
    // This function will prioritize the device whose name matches `default_device`.
    pub async fn find_available_device(&self, default_device: &str) -> Result<Option<String>> {
        let devices = self.spotify.device().await?.into_iter().collect::<Vec<_>>();
        tracing::info!("Available devices: {devices:?}");

        // convert a vector of `Device` items into `(name, id)` items
        let mut devices = devices
            .into_iter()
            .filter_map(|d| d.id.map(|id| (d.name, id)))
            .collect::<Vec<_>>();

        // Manually append the integrated device to the device list if `streaming` feature is enabled.
        // The integrated device may not show up in the device list returned by the Spotify API because
        // 1. The device is just initialized and hasn't been registered in Spotify server.
        //    Related issue/discussion: https://github.com/aome510/spotify-player/issues/79
        // 2. The device list is empty. This is because user doesn't specify their own client ID.
        //    By default, the application uses Spotify web app's client ID, which doesn't have
        //    access to user's active devices.
        #[cfg(feature = "streaming")]
        {
            if let Some(ref session) = self.spotify.session {
                devices.push((
                    self.spotify.device.name.clone(),
                    session.device_id().to_string(),
                ))
            }
        }

        if devices.is_empty() {
            return Ok(None);
        }

        let id = if let Some(id) = devices.iter().position(|d| d.0 == default_device) {
            // prioritize the default device (specified in the app configs) if available
            id
        } else {
            // else, use the first available device
            0
        };

        Ok(Some(devices.remove(id).1))
    }

    /// gets the saved (liked) tracks of the current user
    pub async fn current_user_saved_tracks(&self) -> Result<Vec<Track>> {
        let first_page = self
            .spotify
            .current_user_saved_tracks_manual(None, Some(50), None)
            .await?;

        let tracks = self.all_paging_items(first_page).await?;
        Ok(tracks
            .into_iter()
            .filter_map(|t| Track::try_from_full_track(t.track))
            .collect())
    }

    /// gets the recently played tracks of the current user
    pub async fn current_user_recently_played_tracks(&self) -> Result<Vec<Track>> {
        let first_page = self
            .spotify
            .current_user_recently_played(Some(50), None)
            .await?;

        let play_histories = self.all_cursor_based_paging_items(first_page).await?;

        // de-duplicate the tracks returned from the recently-played API
        let mut tracks = Vec::<Track>::new();
        for history in play_histories {
            if !tracks.iter().any(|t| t.name == history.track.name) {
                if let Some(track) = Track::try_from_full_track(history.track) {
                    tracks.push(track);
                }
            }
        }
        Ok(tracks)
    }

    /// gets the top tracks of the current user
    pub async fn current_user_top_tracks(&self) -> Result<Vec<Track>> {
        let first_page = self
            .spotify
            .current_user_top_tracks_manual(None, Some(50), None)
            .await?;

        let tracks = self.all_paging_items(first_page).await?;
        Ok(tracks
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect())
    }

    /// gets all playlists of the current user
    pub async fn current_user_playlists(&self) -> Result<Vec<Playlist>> {
        let first_page = self
            .spotify
            .current_user_playlists_manual(Some(50), None)
            .await?;

        let playlists = self.all_paging_items(first_page).await?;
        Ok(playlists.into_iter().map(|p| p.into()).collect())
    }

    /// gets all followed artists of the current user
    pub async fn current_user_followed_artists(&self) -> Result<Vec<Artist>> {
        let first_page = self
            .spotify
            .current_user_followed_artists(None, None)
            .await?;

        // followed artists pagination is handled different from
        // other paginations. The endpoint uses cursor-based pagination.
        let mut artists = first_page.items;
        let mut maybe_next = first_page.next;
        while let Some(url) = maybe_next {
            let mut next_page = self
                .internal_call::<rspotify_model::CursorPageFullArtists>(&url)
                .await?
                .artists;
            artists.append(&mut next_page.items);
            maybe_next = next_page.next;
        }

        // converts `rspotify_model::FullArtist` into `state::Artist`
        Ok(artists.into_iter().map(|a| a.into()).collect())
    }

    /// gets all saved albums of the current user
    pub async fn current_user_saved_albums(&self) -> Result<Vec<Album>> {
        let first_page = self
            .spotify
            .current_user_saved_albums_manual(None, Some(50), None)
            .await?;

        let albums = self.all_paging_items(first_page).await?;

        // converts `rspotify_model::SavedAlbum` into `state::Album`
        Ok(albums.into_iter().map(|a| a.album.into()).collect())
    }

    /// gets all albums of an artist
    pub async fn artist_albums(&self, artist_id: &ArtistId) -> Result<Vec<Album>> {
        let mut singles = {
            let first_page = self
                .spotify
                .artist_albums_manual(
                    artist_id,
                    Some(&rspotify_model::AlbumType::Single),
                    None,
                    Some(50),
                    None,
                )
                .await?;
            self.all_paging_items(first_page).await
        }?;
        let mut albums = {
            let first_page = self
                .spotify
                .artist_albums_manual(
                    artist_id,
                    Some(&rspotify_model::AlbumType::Album),
                    None,
                    Some(50),
                    None,
                )
                .await?;
            self.all_paging_items(first_page).await
        }?;
        albums.append(&mut singles);

        // converts `rspotify_model::SimplifiedAlbum` into `state::Album`
        let albums = albums
            .into_iter()
            .filter_map(Album::try_from_simplified_album)
            .collect();
        Ok(self.clean_up_artist_albums(albums))
    }

    /// starts a playback
    pub async fn start_playback(&self, playback: Playback, device_id: Option<&str>) -> Result<()> {
        match playback {
            Playback::Context(id, offset) => match id {
                ContextId::Album(id) => {
                    self.spotify
                        .start_context_playback(&id, device_id, offset, None)
                        .await?
                }
                ContextId::Artist(id) => {
                    self.spotify
                        .start_context_playback(&id, device_id, offset, None)
                        .await?
                }
                ContextId::Playlist(id) => {
                    self.spotify
                        .start_context_playback(&id, device_id, offset, None)
                        .await?
                }
            },
            Playback::URIs(track_ids, offset) => {
                self.spotify
                    .start_uris_playback(
                        track_ids
                            .iter()
                            .map(|id| id as &dyn rspotify_model::PlayableId)
                            .collect::<Vec<_>>(),
                        device_id,
                        offset,
                        None,
                    )
                    .await?
            }
        }

        Ok(())
    }

    /// gets recommendation tracks from a recommendation seed
    pub async fn recommendations(&self, seed: &SeedItem) -> Result<Vec<Track>> {
        let attributes = vec![];

        let tracks = match seed {
            SeedItem::Artist(artist) => {
                self.spotify
                    .recommendations(
                        attributes,
                        Some(vec![&artist.id]),
                        None::<Vec<_>>,
                        None::<Vec<_>>,
                        None,
                        Some(50),
                    )
                    .await?
                    .tracks
            }
            SeedItem::Track(track) => {
                self.spotify
                    .recommendations(
                        attributes,
                        Some(track.artists.iter().map(|a| &a.id).collect::<Vec<_>>()),
                        None::<Vec<_>>,
                        Some(vec![&track.id]),
                        None,
                        Some(50),
                    )
                    .await?
                    .tracks
            }
        };

        let mut tracks = tracks
            .into_iter()
            .filter_map(Track::try_from_simplified_track)
            .collect::<Vec<_>>();

        // for track recommendation seed, add the track seed to the returned recommended tracks
        if let SeedItem::Track(track) = seed {
            let mut seed_track = track.clone();
            // recommended tracks returned from the API are represented by `SimplifiedTrack` struct,
            // which doesn't have `album` field specified. So, we need to change the seed track's
            // `album` field for consistency with other tracks in the list.
            seed_track.album = None;
            tracks.insert(0, seed_track);
        }

        Ok(tracks)
    }

    /// searchs for items (tracks, artists, albums, playlists) that match a given query string.
    pub async fn search(&self, query: &str) -> Result<SearchResults> {
        let (track_result, artist_result, album_result, playlist_result) = tokio::try_join!(
            self.search_specific_type(query, &rspotify_model::SearchType::Track),
            self.search_specific_type(query, &rspotify_model::SearchType::Artist),
            self.search_specific_type(query, &rspotify_model::SearchType::Album),
            self.search_specific_type(query, &rspotify_model::SearchType::Playlist)
        )?;

        let (tracks, artists, albums, playlists) = (
            match track_result {
                rspotify_model::SearchResult::Tracks(p) => p
                    .items
                    .into_iter()
                    .filter_map(Track::try_from_full_track)
                    .collect(),
                _ => anyhow::bail!("expect a track search result"),
            },
            match artist_result {
                rspotify_model::SearchResult::Artists(p) => {
                    p.items.into_iter().map(|a| a.into()).collect()
                }
                _ => anyhow::bail!("expect an artist search result"),
            },
            match album_result {
                rspotify_model::SearchResult::Albums(p) => p
                    .items
                    .into_iter()
                    .filter_map(Album::try_from_simplified_album)
                    .collect(),
                _ => anyhow::bail!("expect an album search result"),
            },
            match playlist_result {
                rspotify_model::SearchResult::Playlists(p) => {
                    p.items.into_iter().map(|i| i.into()).collect()
                }
                _ => anyhow::bail!("expect a playlist search result"),
            },
        );

        Ok(SearchResults {
            tracks,
            artists,
            albums,
            playlists,
        })
    }

    async fn search_specific_type(
        &self,
        query: &str,
        _type: &rspotify_model::SearchType,
    ) -> Result<rspotify_model::SearchResult> {
        Ok(self
            .spotify
            .search(query, _type, None, None, None, None)
            .await?)
    }

    /// adds track to queue
    pub async fn add_track_to_queue(&self, track_id: &TrackId) -> Result<()> {
        Ok(self.spotify.add_item_to_queue(track_id, None).await?)
    }

    /// adds track to a playlist
    pub async fn add_track_to_playlist(
        &self,
        state: &SharedState,
        playlist_id: &PlaylistId,
        track_id: &TrackId,
    ) -> Result<()> {
        let dyn_track_id = track_id as &dyn PlayableId;

        // remove all the occurrences of the track to ensure no duplication in the playlist
        self.spotify
            .playlist_remove_all_occurrences_of_items(playlist_id, vec![dyn_track_id], None)
            .await?;

        self.spotify
            .playlist_add_items(playlist_id, vec![dyn_track_id], None)
            .await?;

        // After adding a new track to a playlist, remove the cache of that playlist to force refetching new data
        state.data.write().caches.context.pop(&playlist_id.uri());

        Ok(())
    }

    /// removes a track from a playlist
    pub async fn delete_track_from_playlist(
        &self,
        state: &SharedState,
        playlist_id: &PlaylistId,
        track_id: &TrackId,
    ) -> Result<()> {
        let dyn_track_id = track_id as &dyn PlayableId;

        // remove all the occurrences of the track to ensure no duplication in the playlist
        self.spotify
            .playlist_remove_all_occurrences_of_items(playlist_id, vec![dyn_track_id], None)
            .await?;

        // After making a delete request, update the playlist in-memory data stored inside the app caches.
        if let Some(Context::Playlist { tracks, .. }) = state
            .data
            .write()
            .caches
            .context
            .get_mut(&playlist_id.uri())
        {
            tracks.retain(|t| t.id != *track_id);
        }

        Ok(())
    }

    /// adds a Spotify item to current user's library.
    /// Before adding new item, the function checks if that item already exists in the library
    /// to avoid adding a duplicated item.
    pub async fn add_to_library(&self, state: &SharedState, item: Item) -> Result<()> {
        match item {
            Item::Track(track) => {
                let contains = self
                    .spotify
                    .current_user_saved_tracks_contains(vec![&track.id])
                    .await?;
                if !contains[0] {
                    self.spotify
                        .current_user_saved_tracks_add(vec![&track.id])
                        .await?;
                    // update the in-memory `user_data`
                    state.data.write().user_data.saved_tracks.insert(0, track);
                }
            }
            Item::Album(album) => {
                let contains = self
                    .spotify
                    .current_user_saved_albums_contains(vec![&album.id])
                    .await?;
                if !contains[0] {
                    self.spotify
                        .current_user_saved_albums_add(vec![&album.id])
                        .await?;
                    // update the in-memory `user_data`
                    state.data.write().user_data.saved_albums.insert(0, album);
                }
            }
            Item::Artist(artist) => {
                let follows = self
                    .spotify
                    .user_artist_check_follow(vec![&artist.id])
                    .await?;
                if !follows[0] {
                    self.spotify.user_follow_artists(vec![&artist.id]).await?;
                    // update the in-memory `user_data`
                    state
                        .data
                        .write()
                        .user_data
                        .followed_artists
                        .insert(0, artist);
                }
            }
            Item::Playlist(playlist) => {
                let user_id = state
                    .data
                    .read()
                    .user_data
                    .user
                    .as_ref()
                    .map(|u| u.id.clone());

                if let Some(user_id) = user_id {
                    let follows = self
                        .spotify
                        .playlist_check_follow(&playlist.id, &[&user_id])
                        .await?;
                    if !follows[0] {
                        self.spotify.playlist_follow(&playlist.id, None).await?;
                        // update the in-memory `user_data`
                        state.data.write().user_data.playlists.insert(0, playlist);
                    }
                }
            }
        }
        Ok(())
    }

    // deletes a Spotify item from user's library
    pub async fn delete_from_library(&self, state: &SharedState, id: ItemId) -> Result<()> {
        match id {
            ItemId::Track(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .saved_tracks
                    .retain(|t| t.id != id);
                self.spotify.current_user_saved_tracks_delete(&[id]).await?;
            }
            ItemId::Album(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .saved_albums
                    .retain(|a| a.id != id);
                self.spotify.current_user_saved_albums_delete(&[id]).await?;
            }
            ItemId::Artist(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .followed_artists
                    .retain(|a| a.id != id);
                self.spotify.user_unfollow_artists(&[id]).await?;
            }
            ItemId::Playlist(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .playlists
                    .retain(|p| p.id != id);
                self.spotify.playlist_unfollow(&id).await?;
            }
        }
        Ok(())
    }

    /// gets a playlist context data
    async fn playlist_context(&self, playlist_id: &PlaylistId) -> Result<Context> {
        let playlist_uri = playlist_id.uri();
        tracing::info!("Get playlist context: {}", playlist_uri);

        let playlist = self.spotify.playlist(playlist_id, None, None).await?;

        // get the playlist's tracks
        let first_page = playlist.tracks.clone();
        let tracks = self
            .all_paging_items(first_page)
            .await?
            .into_iter()
            .filter_map(|item| match item.track {
                Some(rspotify_model::PlayableItem::Track(track)) => {
                    Track::try_from_full_track(track)
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        Ok(Context::Playlist {
            playlist: playlist.into(),
            tracks,
        })
    }

    /// gets an album context data
    async fn album_context(&self, album_id: &AlbumId) -> Result<Context> {
        let album_uri = album_id.uri();
        tracing::info!("Get album context: {}", album_uri);

        let album = self.spotify.album(album_id).await?;
        let first_page = album.tracks.clone();

        // converts `rspotify_model::FullAlbum` into `state::Album`
        let album: Album = album.into();

        // get the album's tracks
        let tracks = self
            .all_paging_items(first_page)
            .await?
            .into_iter()
            .filter_map(|t| {
                // simplified track doesn't have album so
                // we need to manually include one during
                // converting into `state::Track`
                Track::try_from_simplified_track(t).map(|mut t| {
                    t.album = Some(album.clone());
                    t
                })
            })
            .collect::<Vec<_>>();

        Ok(Context::Album { album, tracks })
    }

    /// gets an artist context data
    async fn artist_context(&self, artist_id: &ArtistId) -> Result<Context> {
        let artist_uri = artist_id.uri();
        tracing::info!("Get artist context: {}", artist_uri);

        // get the artist's information, top tracks, related artists and albums
        let artist = self.spotify.artist(artist_id).await?.into();

        let top_tracks = self
            .spotify
            .artist_top_tracks(artist_id, &rspotify_model::enums::misc::Market::FromToken)
            .await?
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect::<Vec<_>>();

        let related_artists = self
            .spotify
            .artist_related_artists(artist_id)
            .await?
            .into_iter()
            .map(|a| a.into())
            .collect::<Vec<_>>();

        let albums = self.artist_albums(artist_id).await?;

        Ok(Context::Artist {
            artist,
            top_tracks,
            albums,
            related_artists,
        })
    }

    /// calls a GET HTTP request to the Spotify server,
    /// and parses the response into a specific type `T`.
    async fn internal_call<T>(&self, url: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let access_token = self.spotify.access_token().await?;
        Ok(self
            .http
            .get(url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", access_token),
            )
            .send()
            .await?
            .json::<T>()
            .await?)
    }

    /// gets all paging items starting from a pagination object of the first page
    async fn all_paging_items<T>(&self, first_page: rspotify_model::Page<T>) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut items = first_page.items;
        let mut maybe_next = first_page.next;
        while let Some(url) = maybe_next {
            let mut next_page = self.internal_call::<rspotify_model::Page<T>>(&url).await?;
            items.append(&mut next_page.items);
            maybe_next = next_page.next;
        }
        Ok(items)
    }

    /// gets all cursor-based paging items starting from a pagination object of the first page
    async fn all_cursor_based_paging_items<T>(
        &self,
        first_page: rspotify_model::CursorBasedPage<T>,
    ) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut items = first_page.items;
        let mut maybe_next = first_page.next;
        while let Some(url) = maybe_next {
            let mut next_page = self
                .internal_call::<rspotify_model::CursorBasedPage<T>>(&url)
                .await?;
            items.append(&mut next_page.items);
            maybe_next = next_page.next;
        }
        Ok(items)
    }

    /// updates the current playback state
    pub async fn update_current_playback_state(&self, state: &SharedState) -> Result<()> {
        let playback = self.spotify.current_playback(None, None::<Vec<_>>).await?;
        let mut player = state.player.write();

        player.playback = playback;
        player.playback_last_updated_time = Some(std::time::Instant::now());

        Ok(())
    }

    #[cfg(feature = "image")]
    pub async fn get_current_track_cover_image(&self, state: &SharedState) -> Result<()> {
        let url = state
            .player
            .read()
            .current_playing_track_album_cover_url()
            .map(String::from);

        if let Some(url) = url {
            if !state.data.read().caches.images.contains(&url) {
                tracing::info!("Retrieving an image from url: {url}");

                // Get the image from a url
                let image = {
                    let image_bytes = self
                        .http
                        .get(&url)
                        .send()
                        .await
                        .context(format!("Failed to get image data from url {url}"))?
                        .bytes()
                        .await?;

                    image::load_from_memory(&image_bytes)
                        .context("Failed to load image from memory")?
                };

                // Update the caches, so we don't have to make the same request multiple times.
                state.data.write().caches.images.put(url, image);
            }
        }

        Ok(())
    }

    /// cleans up a list of albums, which includes
    /// - sort albums by the release date
    /// - remove albums with duplicated names
    fn clean_up_artist_albums(&self, albums: Vec<Album>) -> Vec<Album> {
        let mut albums = albums.into_iter().collect::<Vec<_>>();

        albums.sort_by(|x, y| x.release_date.partial_cmp(&y.release_date).unwrap());

        // use a HashSet to keep track albums with the same name
        let mut seen_names = std::collections::HashSet::new();

        albums.into_iter().rfold(vec![], |mut acc, a| {
            if !seen_names.contains(&a.name) {
                seen_names.insert(a.name.clone());
                acc.push(a);
            }
            acc
        })
    }
}
