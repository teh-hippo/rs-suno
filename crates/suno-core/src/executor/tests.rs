use super::*;
use crate::ClerkAuth;
use crate::http::HttpResponse;
use crate::testutil::{MemFs, RecordingClock, Reply, ScriptedHttp, StubFfmpeg};

fn clip(id: &str) -> Clip {
    Clip {
        id: id.to_owned(),
        title: "Song".to_owned(),
        audio_url: format!("https://cdn1.suno.ai/{id}.mp3"),
        ..Default::default()
    }
}

fn art_clip(id: &str) -> Clip {
    Clip {
        image_large_url: format!("https://art.suno.ai/{id}/large.jpg"),
        image_url: format!("https://art.suno.ai/{id}/small.jpg"),
        ..clip(id)
    }
}

fn desired(clip: Clip, format: AudioFormat) -> Desired {
    Desired {
        path: format!("{}.{}", clip.id, format.ext()),
        lineage: LineageContext::own_root(&clip),
        clip,
        format,
        meta_hash: "m".to_owned(),
        art_hash: "art".to_owned(),
        embedded_lyrics_hash: String::new(),
        modes: vec![SourceMode::Mirror],
        trashed: false,
        private: false,
        artifacts: Vec::new(),
        stems: None,
    }
}

fn entry(path: &str, format: AudioFormat) -> ManifestEntry {
    ManifestEntry {
        path: path.to_owned(),
        format,
        meta_hash: "old".to_owned(),
        art_hash: "old-art".to_owned(),
        size: 8,
        preserve: false,
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn run<G: Ffmpeg>(
    plan: &Plan,
    manifest: &mut Manifest,
    desired: &[Desired],
    http: &ScriptedHttp,
    fs: &MemFs,
    ffmpeg: &G,
    clock: &RecordingClock,
    opts: &ExecOptions,
) -> ExecOutcome {
    let mut albums = BTreeMap::new();
    run_with_albums(
        plan,
        manifest,
        &mut albums,
        desired,
        http,
        fs,
        ffmpeg,
        clock,
        opts,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with_albums<G: Ffmpeg>(
    plan: &Plan,
    manifest: &mut Manifest,
    albums: &mut BTreeMap<String, AlbumArt>,
    desired: &[Desired],
    http: &ScriptedHttp,
    fs: &MemFs,
    ffmpeg: &G,
    clock: &RecordingClock,
    opts: &ExecOptions,
) -> ExecOutcome {
    let mut playlists = BTreeMap::new();
    run_full(
        plan,
        manifest,
        albums,
        &mut playlists,
        desired,
        http,
        fs,
        ffmpeg,
        clock,
        opts,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_full<G: Ffmpeg>(
    plan: &Plan,
    manifest: &mut Manifest,
    albums: &mut BTreeMap<String, AlbumArt>,
    playlists: &mut BTreeMap<String, PlaylistState>,
    desired: &[Desired],
    http: &ScriptedHttp,
    fs: &MemFs,
    ffmpeg: &G,
    clock: &RecordingClock,
    opts: &ExecOptions,
) -> ExecOutcome {
    let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
    let synced = HashMap::new();
    pollster::block_on(execute(
        plan,
        manifest,
        albums,
        playlists,
        desired,
        &synced,
        Ports {
            client: &client,
            http,
            fs,
            ffmpeg,
            clock,
        },
        opts,
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_with_synced<G: Ffmpeg>(
    plan: &Plan,
    manifest: &mut Manifest,
    desired: &[Desired],
    synced: &HashMap<String, AlignedLyrics>,
    http: &ScriptedHttp,
    fs: &MemFs,
    ffmpeg: &G,
    clock: &RecordingClock,
    opts: &ExecOptions,
) -> ExecOutcome {
    let mut albums = BTreeMap::new();
    let mut playlists = BTreeMap::new();
    let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
    pollster::block_on(execute(
        plan,
        manifest,
        &mut albums,
        &mut playlists,
        desired,
        synced,
        Ports {
            client: &client,
            http,
            fs,
            ffmpeg,
            clock,
        },
        opts,
    ))
}

fn small_poll() -> ExecOptions {
    ExecOptions {
        max_retries: 3,
        wav_poll_attempts: 2,
        wav_poll_interval: Duration::from_secs(5),
        concurrency: 4,
        embed_animated_cover: false,
        cover_webp: WebpEncodeSettings::default(),
    }
}

fn fs_new() -> MemFs {
    MemFs::new()
}

mod actions;
mod album_art;
mod artifacts;
mod audio_cover;
mod audio_format;
mod concurrency;
mod failures;
mod stems;
