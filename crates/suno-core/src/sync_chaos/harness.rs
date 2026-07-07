//! The shared full-sync driver and the clip/world builders every layer uses.
//!
//! A [`ClipSpec`] is the test author's view of one remote clip. The harness
//! turns a set of specs into the [`Desired`] selection the engine consumes (path
//! via a deterministic namer, content hashes via the real [`meta_hash`] and
//! [`art_hash`] sentinels) and into a [`ChaosHttp`] "origin" that serves the
//! bytes a download needs. [`run_sync`] then probes the in-memory disk, runs
//! [`reconcile`], and applies the plan through [`execute`], exactly as the CLI
//! would, so a whole sync happens in memory with no real IO.

use std::collections::HashMap;
use std::time::Duration;

use crate::auth::ClerkAuth;
use crate::client::SunoClient;
use crate::executor::{ExecOptions, ExecOutcome, Ports, execute};
use crate::fs::Filesystem;
use crate::hash::{art_hash, meta_hash};
use crate::lineage::LineageContext;
use crate::manifest::Manifest;
use crate::model::Clip;
use crate::reconcile::{Action, Desired, LocalFile, Plan, SourceStatus, reconcile};
use crate::testutil::{ChaosHttp, MemFs, Outcome, RecordingClock, StubFfmpeg};
use crate::vocab::WebpEncodeSettings;
use crate::vocab::{AudioFormat, SourceMode};

/// A test author's description of one remote clip.
///
/// The fields chosen here are exactly the ones that drive engine decisions: the
/// title feeds the path (so changing it forces a rename) and the embedded tags,
/// the creator feeds both the path and the artist tag, the tags and art feed the
/// content hashes (so changing them forces a retag), and the modes, trashed, and
/// private flags drive the deletion guards.
#[derive(Clone, Debug)]
pub(super) struct ClipSpec {
    pub id: String,
    pub title: String,
    /// The account display name; feeds both the path (creator folder) and the
    /// embedded artist tag, so changing it forces a rename and a retag.
    pub creator: String,
    /// Feeds `meta_hash`; bump to force a retag.
    pub tags: String,
    /// The large cover-art URL; empty means no art. Feeds `art_hash`.
    pub art: String,
    pub format: AudioFormat,
    pub modes: Vec<SourceMode>,
    pub trashed: bool,
    pub private: bool,
}

impl ClipSpec {
    /// A plain mirror-held MP3 clip with the given id and title. The MP3 path
    /// keeps the harness HTTP simple: a single public GET, no auth or render.
    pub(super) fn mirror(id: &str, title: &str) -> Self {
        Self {
            id: id.to_owned(),
            title: title.to_owned(),
            creator: "Artist".to_owned(),
            tags: format!("tag-{id}"),
            art: format!("https://cdn1.suno.ai/{id}-art.jpeg"),
            format: AudioFormat::Mp3,
            modes: vec![SourceMode::Mirror],
            trashed: false,
            private: false,
        }
    }

    pub(super) fn with_format(mut self, format: AudioFormat) -> Self {
        self.format = format;
        self
    }

    pub(super) fn copy_held(mut self) -> Self {
        if !self.modes.contains(&SourceMode::Copy) {
            self.modes.push(SourceMode::Copy);
        }
        self
    }

    pub(super) fn private(mut self) -> Self {
        self.private = true;
        self
    }

    pub(super) fn trashed(mut self) -> Self {
        self.trashed = true;
        self
    }

    pub(super) fn with_tags(mut self, tags: &str) -> Self {
        self.tags = tags.to_owned();
        self
    }

    pub(super) fn with_title(mut self, title: &str) -> Self {
        self.title = title.to_owned();
        self
    }

    pub(super) fn with_creator(mut self, creator: &str) -> Self {
        self.creator = creator.to_owned();
        self
    }
}

/// Reduce a title to a path-safe slug, so a title change yields a path change.
fn slug(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "untitled".to_owned()
    } else {
        cleaned
    }
}

/// The deterministic relative path for a spec: a creator slug, then a title
/// slug plus the id (so it is unique per clip and stable across runs) and the
/// format extension. The creator feeds both the path and the embedded artist
/// tag, so changing either the creator or the title forces a rename plus a
/// retag.
pub(super) fn path_of(spec: &ClipSpec) -> String {
    format!(
        "{}/{}-{}.{}",
        slug(&spec.creator),
        slug(&spec.title),
        spec.id,
        spec.format.ext()
    )
}

/// Build the [`Clip`] a spec stands for. URLs are derived from the id so the
/// origin and the content hashes agree on exactly one set of addresses.
pub(super) fn clip_of(spec: &ClipSpec) -> Clip {
    Clip {
        id: spec.id.clone(),
        title: spec.title.clone(),
        tags: spec.tags.clone(),
        display_name: spec.creator.clone(),
        audio_url: format!("https://cdn1.suno.ai/{}.mp3", spec.id),
        image_large_url: spec.art.clone(),
        ..Default::default()
    }
}

/// Build the [`Desired`] selection entry for a spec, using the real content
/// sentinels so retag detection is exercised exactly as in production.
pub(super) fn desired_of(spec: &ClipSpec) -> Desired {
    let clip = clip_of(spec);
    let lineage = LineageContext::own_root(&clip);
    Desired {
        path: path_of(spec),
        format: spec.format,
        meta_hash: meta_hash(&clip, &lineage),
        art_hash: art_hash(&clip),
        modes: spec.modes.clone(),
        trashed: spec.trashed,
        private: spec.private,
        lineage,
        clip,
        artifacts: Vec::new(),
        stems: None,
    }
}

/// The whole desired selection for a set of specs.
pub(super) fn desired_set(specs: &[ClipSpec]) -> Vec<Desired> {
    specs.iter().map(desired_of).collect()
}

/// Stand-in audio bytes for an id (the raw MP3/WAV source body).
fn audio_bytes(id: &str) -> Vec<u8> {
    format!("audio-source-for-{id}").into_bytes()
}

/// Stand-in cover-art bytes for an art URL.
fn art_bytes(url: &str) -> Vec<u8> {
    format!("art-bytes-for-{url}").into_bytes()
}

/// The `wav_file` poll body advertising a ready render for an id.
fn wav_file_json(id: &str) -> String {
    format!(r#"{{"wav_file_url": "https://cdn1.suno.ai/{id}.wav"}}"#)
}

/// Build a clean origin [`ChaosHttp`] that serves every byte the given specs
/// need, with no faults: MP3 audio, the WAV render flow for FLAC/WAV clips, and
/// cover art. Route keys are full URLs over fixed-width ids, so no key is a
/// substring of another and each request resolves to exactly its own clip.
pub(super) fn world(specs: &[ClipSpec]) -> ChaosHttp {
    let mut http = ChaosHttp::new()
        .with_auth()
        .program("/convert_wav/", vec![Outcome::status(200)]);
    for spec in specs {
        let id = &spec.id;
        match spec.format {
            AudioFormat::Mp3 => {
                http = http.serve(&format!("/{id}.mp3"), audio_bytes(id));
            }
            AudioFormat::Flac | AudioFormat::Wav | AudioFormat::Alac => {
                http = http
                    .serve(
                        &format!("gen/{id}/wav_file"),
                        wav_file_json(id).into_bytes(),
                    )
                    .serve(&format!("/{id}.wav"), audio_bytes(id));
            }
        }
        if !spec.art.is_empty() {
            http = http.serve(&spec.art, art_bytes(&spec.art));
        }
    }
    http
}

/// One mirror source, fully enumerated: the normal, delete-allowed case.
pub(super) fn clean_mirror() -> Vec<SourceStatus> {
    vec![SourceStatus {
        mode: SourceMode::Mirror,
        fully_enumerated: true,
    }]
}

/// Derive the fully-enumerated source statuses a clean run should present from
/// the modes the specs actually select: always the library mirror, plus a copy
/// source whenever any clip is copy-held. This threads real copy-vs-mirror
/// status through the whole pipeline instead of pretending every run is a lone
/// mirror, so end-to-end runs exercise the same `deletion_allowed` inputs the
/// CLI builds. With every source fully enumerated this is behaviourally a
/// delete-allowed run, exactly like [`clean_mirror`]; the difference shows up
/// only when a test marks a copy source unreliable.
pub(super) fn sources_for(specs: &[ClipSpec]) -> Vec<SourceStatus> {
    let mut sources = clean_mirror();
    if specs.iter().any(|s| s.modes.contains(&SourceMode::Copy)) {
        sources.push(SourceStatus {
            mode: SourceMode::Copy,
            fully_enumerated: true,
        });
    }
    sources
}

/// Fast options: the recording clock never really sleeps, so a tiny poll budget
/// keeps even the FLAC render path instant while still exercising it.
pub(super) fn fast_opts() -> ExecOptions {
    ExecOptions {
        max_retries: 3,
        wav_poll_attempts: 3,
        wav_poll_interval: Duration::from_secs(5),
        concurrency: 4,
        embed_animated_cover: false,
        cover_webp: WebpEncodeSettings::default(),
    }
}

/// Probe the in-memory disk for each manifest path and all tracked artifact
/// paths, building the `local` map [`reconcile`] consumes.  This is the bridge
/// the CLI performs between the persisted manifest and the real filesystem.
pub(super) fn probe_local(manifest: &Manifest, fs: &MemFs) -> HashMap<String, LocalFile> {
    let probe = |path: &str| -> LocalFile {
        match fs.metadata(path) {
            Some(stat) => LocalFile {
                exists: stat.exists,
                size: stat.size,
            },
            None => LocalFile::default(),
        }
    };

    let mut map = HashMap::new();
    for (id, entry) in manifest.iter() {
        // Audio file, keyed by clip_id.
        map.insert(id.clone(), probe(&entry.path));

        // Per-clip sidecars, keyed by their stored path.
        for path in [
            entry.cover_jpg.as_ref().map(|s| s.path.as_str()),
            entry.cover_webp.as_ref().map(|s| s.path.as_str()),
            entry.details_txt.as_ref().map(|s| s.path.as_str()),
            entry.lyrics_txt.as_ref().map(|s| s.path.as_str()),
            entry.lrc.as_ref().map(|s| s.path.as_str()),
            entry.video_mp4.as_ref().map(|s| s.path.as_str()),
        ]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        {
            map.entry(path.to_owned()).or_insert_with(|| probe(path));
        }
    }
    map
}

/// Apply a plan through [`execute`], blocking on the future with the in-memory
/// ports. A fresh client and recording clock are used per run.
pub(super) fn drive(
    plan: &Plan,
    manifest: &mut Manifest,
    desired: &[Desired],
    http: &ChaosHttp,
    fs: &MemFs,
    opts: &ExecOptions,
) -> ExecOutcome {
    let client = SunoClient::new(ClerkAuth::new("eyJtoken"), RecordingClock::new());
    let clock = RecordingClock::new();
    let ffmpeg = StubFfmpeg::flac();
    let mut albums = std::collections::BTreeMap::new();
    let mut playlists = std::collections::BTreeMap::new();
    let synced = std::collections::HashMap::new();
    pollster::block_on(execute(
        plan,
        manifest,
        &mut albums,
        &mut playlists,
        desired,
        &synced,
        Ports {
            client: &client,
            http,
            fs,
            ffmpeg: &ffmpeg,
            clock: &clock,
        },
        opts,
    ))
}

/// Run one full sync: probe the disk, reconcile, and execute. Returns the plan
/// (for plan-level assertions) and the outcome.
pub(super) fn run_sync(
    specs: &[ClipSpec],
    sources: &[SourceStatus],
    fs: &MemFs,
    manifest: &mut Manifest,
    http: &ChaosHttp,
    opts: &ExecOptions,
) -> (Plan, ExecOutcome) {
    let desired = desired_set(specs);
    let local = probe_local(manifest, fs);
    let plan = reconcile(manifest, &desired, &local, sources);
    let outcome = drive(&plan, manifest, &desired, http, fs, opts);
    (plan, outcome)
}

/// Run one clean, fully-enumerated sync against a freshly built clean origin.
/// The source statuses are derived from the specs' modes, so a copy-held set
/// presents a copy source end to end (see [`sources_for`]).
pub(super) fn run_clean(
    specs: &[ClipSpec],
    fs: &MemFs,
    manifest: &mut Manifest,
) -> (Plan, ExecOutcome) {
    let http = world(specs);
    run_sync(
        specs,
        &sources_for(specs),
        fs,
        manifest,
        &http,
        &fast_opts(),
    )
}

/// How many of a plan's actions actually mutate the library (everything but a
/// no-op skip). A converged run has zero.
pub(super) fn mutating_actions(plan: &Plan) -> usize {
    plan.actions
        .iter()
        .filter(|a| !matches!(a, Action::Skip { .. }))
        .count()
}
