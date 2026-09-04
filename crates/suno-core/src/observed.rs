//! Pure observation of the metadata a downloaded audio file already carries.
//!
//! [`observe`] decodes what a file holds *right now* into one container-neutral
//! [`ObservedAudio`], so a caller can decide whether a planned tag write would
//! actually change anything, and can prove afterwards that it wrote what it
//! meant to. It is the read half of [`tag`](crate::tag): the writer turns a
//! [`TrackMetadata`] into container bytes, this turns container bytes back into
//! fields.
//!
//! Three properties shape the module:
//!
//! - **No IO of its own.** Every byte arrives through a caller-supplied
//!   [`AudioSource`] (`Read + Seek`), so the engine stays free of direct file
//!   access and every test runs from an in-memory buffer.
//! - **Metadata only.** No parser reads an audio payload. FLAC stops at the last
//!   metadata block, MP3 reads only the ID3v2 region the header declares, WAV
//!   seeks over `fmt `/`data` to the `ID3 ` chunk, and MP4 seeks over `mdat`.
//! - **Never panics, never leaks.** Arbitrary bytes yield a typed
//!   [`ObserveError`] whose message is fixed text: no input bytes, no field
//!   values, nothing that could echo a pasted token back into a log.
//!
//! Comparison is deliberately caller-driven. [`ManagedTags::differences`] takes
//! an explicit [`ComparePolicy`], because whether an empty value counts as an
//! absent one, and whether a repeated value counts once or twice, are planning
//! decisions this module must not make on a caller's behalf. Nothing here knows
//! about the manifest, reconcile, or the executor.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek, SeekFrom};

use id3::frame::{Content, PictureType, SynchronisedLyricsType, TimestampFormat};

use crate::error::Error;
use crate::hash::content_hash;
use crate::tag::{STATIC_FALLBACK_DESCRIPTION, TrackMetadata};
use crate::vocab::AudioFormat;

/// The iTunes reverse-DNS mean the MP4 writer stores its freeform atoms under.
/// Mirrors the constant in [`tag_alac`](crate::tag_alac); an atom under any
/// other mean is foreign.
const APPLE_ITUNES_MEAN: &str = "com.apple.iTunes";

/// Length of an ID3v2 header, and of its optional footer.
const ID3_HEADER_LEN: usize = 10;

/// Upper bound on a metadata region this module will buffer (64 MiB).
///
/// An ID3 header or a RIFF chunk header is a self-declared length taken from an
/// untrusted file, so a corrupt or hostile one could otherwise ask for an
/// arbitrary allocation. Real tags are orders of magnitude smaller: a FLAC
/// picture block cannot exceed 16 MiB by construction.
const MAX_TAG_REGION_BYTES: u64 = 64 * 1024 * 1024;

/// Guard against a chunk walk that never reaches the end of a malformed RIFF.
const MAX_RIFF_CHUNKS: usize = 4096;

/// Guard against a FLAC block walk that never reaches the last-block flag.
const MAX_FLAC_BLOCKS: usize = 4096;

/// A seekable byte source metadata can be observed from.
///
/// A supertrait of `Read + Seek` with a blanket impl, so a caller can name the
/// one bound (`impl AudioSource`) rather than repeat the pair, and so the
/// requirement that observation needs to *seek* (over PCM data, over `mdat`) is
/// visible in the signatures.
pub trait AudioSource: Read + Seek {}

impl<T: Read + Seek> AudioSource for T {}

/// Whether a metadata region was found at all.
///
/// Distinguishes "this file is fine, it simply carries no tags" from an error:
/// an untagged file is a normal, actionable observation (write the tags), while
/// [`ObserveError`] means the file could not be understood and nothing about its
/// tags may be assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagStatus {
    /// A metadata region was found and decoded. It may still hold no fields.
    Present,
    /// The container parsed but carries no metadata region: an MP3 with no ID3
    /// header, a WAV with no `ID3 ` chunk, a FLAC with no `VORBIS_COMMENT` or
    /// `PICTURE` block, an MP4 with no `ilst` items.
    Absent,
}

/// A metadata field rs-suno writes, and therefore owns, named independently of
/// any container.
///
/// Parsing maps each container's native key onto one of these
/// ([`ManagedField::native_key`] renders the mapping back), so a FLAC `TITLE`,
/// an ID3 `TIT2`, and an MP4 `©nam` compare as the same field. Anything outside
/// this set is foreign and is preserved as a [`ForeignEntry`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ManagedField {
    Title,
    Artist,
    Album,
    AlbumArtist,
    /// The precise `YYYY-MM-DD` creation date.
    Date,
    /// The album release year (`YYYY`).
    Year,
    /// The free-text description (`DESCRIPTION`, `COMM`, `©cmt`).
    Description,
    /// Plain, untimed lyrics.
    Lyrics,
    /// The 1-based track position within its album.
    TrackNumber,
    /// The album's track count.
    TrackTotal,
    /// One of the Suno-specific fields.
    Suno(SunoField),
}

/// A Suno-specific field, stored under the same key in every container: a
/// Vorbis comment, an ID3 `TXXX` description, or an MP4 freeform atom name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SunoField {
    Prompt,
    Style,
    StyleSummary,
    Model,
    Handle,
    Parent,
    Root,
    Lineage,
    Id,
    Url,
}

impl SunoField {
    /// Every Suno field, in the order the tag writer emits them.
    pub const ALL: [SunoField; 10] = [
        SunoField::Prompt,
        SunoField::Style,
        SunoField::StyleSummary,
        SunoField::Model,
        SunoField::Handle,
        SunoField::Parent,
        SunoField::Root,
        SunoField::Lineage,
        SunoField::Id,
        SunoField::Url,
    ];

    /// The container-independent key, matching the tag writer's.
    pub fn key(self) -> &'static str {
        match self {
            SunoField::Prompt => "SUNO_PROMPT",
            SunoField::Style => "SUNO_STYLE",
            SunoField::StyleSummary => "SUNO_STYLE_SUMMARY",
            SunoField::Model => "SUNO_MODEL",
            SunoField::Handle => "SUNO_HANDLE",
            SunoField::Parent => "SUNO_PARENT",
            SunoField::Root => "SUNO_ROOT",
            SunoField::Lineage => "SUNO_LINEAGE",
            SunoField::Id => "SUNO_ID",
            SunoField::Url => "SUNO_URL",
        }
    }

    /// The field a key names, or `None` when it is not a Suno field.
    pub fn from_key(key: &str) -> Option<SunoField> {
        SunoField::ALL.into_iter().find(|field| field.key() == key)
    }
}

impl ManagedField {
    /// Every managed field, standard ones first.
    pub const ALL: [ManagedField; 20] = [
        ManagedField::Title,
        ManagedField::Artist,
        ManagedField::Album,
        ManagedField::AlbumArtist,
        ManagedField::Date,
        ManagedField::Year,
        ManagedField::Description,
        ManagedField::Lyrics,
        ManagedField::TrackNumber,
        ManagedField::TrackTotal,
        ManagedField::Suno(SunoField::Prompt),
        ManagedField::Suno(SunoField::Style),
        ManagedField::Suno(SunoField::StyleSummary),
        ManagedField::Suno(SunoField::Model),
        ManagedField::Suno(SunoField::Handle),
        ManagedField::Suno(SunoField::Parent),
        ManagedField::Suno(SunoField::Root),
        ManagedField::Suno(SunoField::Lineage),
        ManagedField::Suno(SunoField::Id),
        ManagedField::Suno(SunoField::Url),
    ];

    /// The key this field is stored under in `format`, as a caller would see it
    /// in a hex dump: a Vorbis comment name, an ID3 frame id (with the `TXXX`
    /// description or `COMM`/`USLT` slot where one applies), or an MP4 atom.
    ///
    /// Diagnostic only; parsing uses the reverse mapping, which additionally
    /// accepts the aliases documented on [`observe`].
    pub fn native_key(self, format: AudioFormat) -> String {
        match format {
            AudioFormat::Flac => self.vorbis_key().to_owned(),
            AudioFormat::Mp3 | AudioFormat::Wav => match self {
                ManagedField::Title => "TIT2".to_owned(),
                ManagedField::Artist => "TPE1".to_owned(),
                ManagedField::Album => "TALB".to_owned(),
                ManagedField::AlbumArtist => "TPE2".to_owned(),
                ManagedField::Date => "TDRC".to_owned(),
                ManagedField::Year => "TDRL".to_owned(),
                ManagedField::Description => "COMM".to_owned(),
                ManagedField::Lyrics => "USLT".to_owned(),
                ManagedField::TrackNumber | ManagedField::TrackTotal => "TRCK".to_owned(),
                ManagedField::Suno(field) => format!("TXXX:{}", field.key()),
            },
            AudioFormat::Alac => match self {
                ManagedField::Title => "\u{a9}nam".to_owned(),
                ManagedField::Artist => "\u{a9}ART".to_owned(),
                ManagedField::Album => "\u{a9}alb".to_owned(),
                ManagedField::AlbumArtist => "aART".to_owned(),
                ManagedField::Date => format!("----:{APPLE_ITUNES_MEAN}:DATE"),
                ManagedField::Year => "\u{a9}day".to_owned(),
                ManagedField::Description => "\u{a9}cmt".to_owned(),
                ManagedField::Lyrics => "\u{a9}lyr".to_owned(),
                ManagedField::TrackNumber | ManagedField::TrackTotal => "trkn".to_owned(),
                ManagedField::Suno(field) => {
                    format!("----:{APPLE_ITUNES_MEAN}:{}", field.key())
                }
            },
        }
    }

    /// The Vorbis comment name for this field.
    fn vorbis_key(self) -> &'static str {
        match self {
            ManagedField::Title => "TITLE",
            ManagedField::Artist => "ARTIST",
            ManagedField::Album => "ALBUM",
            ManagedField::AlbumArtist => "ALBUMARTIST",
            ManagedField::Date => "DATE",
            ManagedField::Year => "YEAR",
            ManagedField::Description => "DESCRIPTION",
            ManagedField::Lyrics => "LYRICS",
            ManagedField::TrackNumber => "TRACKNUMBER",
            ManagedField::TrackTotal => "TRACKTOTAL",
            ManagedField::Suno(field) => field.key(),
        }
    }

    /// The field an upper-cased Vorbis comment name maps to, or `None` when the
    /// comment is foreign.
    fn from_vorbis(key: &str) -> Option<ManagedField> {
        ManagedField::ALL
            .into_iter()
            .find(|field| field.vorbis_key() == key)
    }
}

/// A canonical, container-neutral set of managed field values.
///
/// Values keep their insertion order within a field (a container may legally
/// repeat a key); comparison normalises that away. The map is kept private so
/// every value that enters it has already been normalised by the parser.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManagedTags {
    values: BTreeMap<ManagedField, Vec<String>>,
}

impl ManagedTags {
    /// An empty set.
    pub fn new() -> ManagedTags {
        ManagedTags::default()
    }

    /// The managed set a tag write derived from `meta` would produce.
    ///
    /// The desired half of a semantic comparison: build this from the metadata
    /// about to be written, [`observe`] the file, and ask
    /// [`differences`](Self::differences) whether a write would change anything.
    ///
    /// An empty value is recorded as an absent field rather than as an empty
    /// one, because the containers disagree on which they store; a caller that
    /// needs the two treated alike on the observed side asks for
    /// [`EmptyPolicy::EmptyIsAbsent`].
    pub fn from_track_metadata(meta: &TrackMetadata) -> ManagedTags {
        let mut tags = ManagedTags::new();
        let standard = [
            (ManagedField::Title, meta.title.as_str()),
            (ManagedField::Artist, meta.artist.as_str()),
            (ManagedField::Album, meta.album.as_str()),
            (ManagedField::AlbumArtist, meta.album_artist.as_str()),
            (ManagedField::Date, meta.date.as_str()),
            (ManagedField::Year, meta.year.as_str()),
            (ManagedField::Description, meta.comment.as_str()),
            (ManagedField::Lyrics, meta.lyrics.as_str()),
        ];
        for (field, value) in standard {
            if !value.is_empty() {
                tags.add(field, value);
            }
        }
        for (field, value) in SunoField::ALL.into_iter().zip(meta.suno_fields()) {
            if !value.1.is_empty() {
                tags.add(ManagedField::Suno(field), value.1);
            }
        }
        if meta.track > 0 {
            tags.add(ManagedField::TrackNumber, meta.track.to_string());
            if meta.track_total > 0 {
                tags.add(ManagedField::TrackTotal, meta.track_total.to_string());
            }
        }
        tags
    }

    /// Append a value to a field, keeping any value already recorded.
    pub fn add(&mut self, field: ManagedField, value: impl Into<String>) {
        self.values.entry(field).or_default().push(value.into());
    }

    /// Replace every value of a field with the one given.
    pub fn set(&mut self, field: ManagedField, value: impl Into<String>) {
        self.values.insert(field, vec![value.into()]);
    }

    /// Every value recorded for a field, in the order observed. Empty when the
    /// field is absent.
    pub fn get(&self, field: ManagedField) -> &[String] {
        self.values.get(&field).map_or(&[], Vec::as_slice)
    }

    /// The first value recorded for a field, or `None` when it is absent.
    pub fn first(&self, field: ManagedField) -> Option<&str> {
        self.get(field).first().map(String::as_str)
    }

    /// Whether a field carries at least one value, empty or not.
    pub fn contains(&self, field: ManagedField) -> bool {
        self.values.contains_key(&field)
    }

    /// Every populated field with its values, in [`ManagedField`] order.
    pub fn fields(&self) -> impl Iterator<Item = (ManagedField, &[String])> {
        self.values
            .iter()
            .map(|(field, values)| (*field, values.as_slice()))
    }

    /// Whether no field is populated.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The fields whose values differ under `policy`, in [`ManagedField`] order.
    ///
    /// Ordering within a field never matters: values are compared as a sorted
    /// multiset, so `[a, b]` equals `[b, a]` while `[a, a]` differs from `[a]`
    /// unless the policy collapses duplicates.
    pub fn differences(&self, other: &ManagedTags, policy: ComparePolicy) -> Vec<ManagedField> {
        let mut differing: Vec<ManagedField> = Vec::new();
        for field in ManagedField::ALL {
            if canonical(self.get(field), policy) != canonical(other.get(field), policy) {
                differing.push(field);
            }
        }
        differing
    }

    /// Whether every managed field matches under `policy`.
    pub fn equivalent(&self, other: &ManagedTags, policy: ComparePolicy) -> bool {
        ManagedField::ALL
            .into_iter()
            .all(|field| canonical(self.get(field), policy) == canonical(other.get(field), policy))
    }
}

/// Whether an empty value counts as an absent one when comparing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyPolicy {
    /// `TITLE=""` and no `TITLE` at all compare equal. Suits a caller comparing
    /// across containers, which disagree on whether an empty field is stored.
    EmptyIsAbsent,
    /// An empty value is a value: `TITLE=""` differs from an absent `TITLE`.
    EmptyIsDistinct,
}

/// Whether a repeated value counts once or as many times as it appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicatePolicy {
    /// Compare as a multiset: two copies of a value differ from one.
    Keep,
    /// Compare as a set: repeats collapse to a single value.
    Collapse,
}

/// How two managed field sets are compared.
///
/// Deliberately has no `Default`: whether an empty value is an absent one, and
/// whether a duplicate matters, are planning decisions belonging to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComparePolicy {
    pub empty: EmptyPolicy,
    pub duplicates: DuplicatePolicy,
}

impl ComparePolicy {
    /// A policy from its two halves.
    pub const fn new(empty: EmptyPolicy, duplicates: DuplicatePolicy) -> ComparePolicy {
        ComparePolicy { empty, duplicates }
    }

    /// The most literal policy: an empty value differs from an absent one, and
    /// duplicates count.
    pub const fn exact() -> ComparePolicy {
        ComparePolicy::new(EmptyPolicy::EmptyIsDistinct, DuplicatePolicy::Keep)
    }

    /// The most forgiving policy: empty is absent, and duplicates collapse.
    pub const fn lenient() -> ComparePolicy {
        ComparePolicy::new(EmptyPolicy::EmptyIsAbsent, DuplicatePolicy::Collapse)
    }
}

/// The comparable form of a field's values: order removed, and empties and
/// duplicates handled per `policy`.
fn canonical(values: &[String], policy: ComparePolicy) -> Vec<&str> {
    let mut canonical: Vec<&str> = values
        .iter()
        .map(String::as_str)
        .filter(|value| policy.empty == EmptyPolicy::EmptyIsDistinct || !value.is_empty())
        .collect();
    canonical.sort_unstable();
    if policy.duplicates == DuplicatePolicy::Collapse {
        canonical.dedup();
    }
    canonical
}

/// An embedded front cover, fingerprinted rather than carried.
///
/// The bytes themselves are of no use to a planner and can be megabytes, so only
/// their length and a stable digest are kept; [`cover_fingerprint`] computes the
/// same digest over an image a caller is about to embed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedCover {
    /// The declared MIME type, lower-cased (`image/jpeg`, `image/webp`). MP4
    /// artwork declares a format rather than a MIME, mapped to the equivalent.
    pub mime: String,
    /// The picture description. Always empty for MP4 artwork, which has none.
    pub description: String,
    /// The image size in bytes.
    pub len: usize,
    /// A stable digest of the image bytes.
    pub fingerprint: String,
}

/// How a file expresses timed lyrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimedLyricsRepresentation {
    /// An ID3 `SYLT` frame whose timestamps are absolute milliseconds: what the
    /// MP3 and WAV writers emit.
    Id3SyltMilliseconds,
    /// An ID3 `SYLT` frame whose timestamps are MPEG frame counts. Read but not
    /// written; the timestamps are not milliseconds and are not comparable with
    /// a millisecond fingerprint.
    Id3SyltMpegFrames,
}

/// Timed lyrics found in a file, summarised.
///
/// The entries themselves are unbounded, so only their shape and a stable
/// fingerprint are kept. [`timed_lyrics_fingerprint`] computes the same digest
/// over the entries a caller is about to write, which makes "is the embedded
/// timing already the timing I would write?" a pure comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedTimedLyrics {
    pub representation: TimedLyricsRepresentation,
    /// The declared language code, as stored.
    pub language: String,
    /// The frame description, usually empty.
    pub description: String,
    /// How many timed entries the frame carries.
    pub entries: usize,
    /// The first and last timestamps, in the representation's own units.
    pub first_timestamp: Option<u32>,
    pub last_timestamp: Option<u32>,
    /// A stable digest of every `(timestamp, text)` pair, in order.
    pub fingerprint: String,
}

/// The value of a metadata entry rs-suno does not manage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForeignValue {
    /// A text value, carried verbatim.
    Text(String),
    /// A binary or structural entry, kept as its length and a stable digest so
    /// a caller can assert it survived a rewrite byte for byte.
    Opaque { len: usize, fingerprint: String },
    /// An entry known to be present whose bytes this module does not decode.
    Present,
}

/// A metadata entry outside the managed set, kept so a caller can assert a
/// rewrite preserved it.
///
/// `key` is the native key as the container spells it: an upper-cased Vorbis
/// comment name, an ID3 frame id (suffixed with its `TXXX`/`COMM`/`USLT`
/// description where it has one), or an MP4 atom (`----:mean:name` for a
/// freeform one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignEntry {
    pub key: String,
    pub value: ForeignValue,
}

/// Everything observation could learn about a file's metadata, expressed
/// independently of the container it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedAudio {
    /// The format the file was parsed as.
    pub format: AudioFormat,
    /// Whether a metadata region was found at all.
    pub status: TagStatus,
    /// The managed fields, normalised across containers.
    pub managed: ManagedTags,
    /// Foreign entries, sorted by key then by rendered value, so two
    /// observations of the same file compare and fingerprint identically.
    pub foreign: Vec<ForeignEntry>,
    /// The embedded front cover, when there is one.
    pub cover: Option<ObservedCover>,
    /// The managed static JPEG fallback stored beside an animated front cover.
    pub static_fallback: Option<ObservedCover>,
    /// The embedded timed lyrics, when there are any.
    pub timed_lyrics: Option<ObservedTimedLyrics>,
    /// How many native metadata entries were seen: one per Vorbis comment
    /// value, ID3 frame, MP4 metadata item, or non-padding FLAC metadata block.
    /// A file whose entry count moves has genuinely gained or lost a field.
    pub entry_count: usize,
    /// The decoded-audio signature the container records, when it records one:
    /// the FLAC `STREAMINFO` MD5, lower-case hex. `None` for every other format,
    /// and for a FLAC that leaves it unset (all zero). Reading it costs nothing
    /// and proves a rewrite left the audio alone.
    pub audio_signature: Option<String>,
}

impl ObservedAudio {
    /// An observation of a valid container with no metadata region.
    fn untagged(format: AudioFormat) -> ObservedAudio {
        ObservedAudio {
            format,
            status: TagStatus::Absent,
            managed: ManagedTags::new(),
            foreign: Vec::new(),
            cover: None,
            static_fallback: None,
            timed_lyrics: None,
            entry_count: 0,
            audio_signature: None,
        }
    }

    /// Whether a metadata region was found.
    pub fn is_tagged(&self) -> bool {
        self.status == TagStatus::Present
    }

    /// The verified fingerprint of the managed cover set.
    pub fn managed_cover_fingerprint(&self) -> Option<String> {
        let primary = self.cover.as_ref()?;
        Some(match self.static_fallback.as_ref() {
            Some(fallback) => content_hash(&format!(
                "primary={}\nstatic={}",
                primary.fingerprint, fallback.fingerprint
            )),
            None => primary.fingerprint.clone(),
        })
    }

    /// The first value of a managed field, or `None` when it is absent.
    pub fn first(&self, field: ManagedField) -> Option<&str> {
        self.managed.first(field)
    }

    /// The embedded plain lyrics, or `None` when the file carries none.
    pub fn lyrics(&self) -> Option<&str> {
        self.managed.first(ManagedField::Lyrics)
    }

    /// The 1-based track position, or `None` when it is absent or not a number.
    ///
    /// Normalises the containers' spellings: an ID3 `TRCK` of `"7/12"`, a Vorbis
    /// `TRACKNUMBER` of `"07"`, and an MP4 `trkn` of `(7, 12)` all read as `7`.
    pub fn track(&self) -> Option<u32> {
        self.managed
            .first(ManagedField::TrackNumber)
            .and_then(|value| value.parse().ok())
    }

    /// The album's track count, or `None` when it is absent or not a number.
    pub fn track_total(&self) -> Option<u32> {
        self.managed
            .first(ManagedField::TrackTotal)
            .and_then(|value| value.parse().ok())
    }

    /// A stable digest of every foreign entry.
    ///
    /// Two observations that agree on this agree on every unmanaged field they
    /// could see, which is what a "the rewrite preserved everything it does not
    /// own" assertion needs.
    pub fn foreign_fingerprint(&self) -> String {
        let mut rendered = String::new();
        for entry in &self.foreign {
            rendered.push_str(&entry.key);
            rendered.push('\u{1}');
            match &entry.value {
                ForeignValue::Text(text) => {
                    rendered.push_str("text:");
                    rendered.push_str(text);
                }
                ForeignValue::Opaque { len, fingerprint } => {
                    rendered.push_str(&format!("opaque:{len}:{fingerprint}"));
                }
                ForeignValue::Present => rendered.push_str("present"),
            }
            rendered.push('\u{0}');
        }
        content_hash(&rendered)
    }
}

/// Why an observation failed.
///
/// Every variant's message is fixed text. Nothing from the file, and no
/// third-party parser's message, reaches it: an `id3` decoding error carries the
/// undecodable bytes, and a metadata value could be anything at all, including a
/// pasted credential, so none of it may reach a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ObserveErrorKind {
    /// The stream is not in the container format it was read as.
    #[error("the stream is not in that container format")]
    NotThisFormat,
    /// The container structure is broken: a truncated header, a length that
    /// does not fit, a block that could not be parsed.
    #[error("the container structure is malformed")]
    Malformed,
    /// The metadata region was located but could not be decoded.
    #[error("the metadata region could not be decoded")]
    UnreadableTags,
    /// The metadata region declares a length past what will be buffered.
    #[error("the metadata region is larger than the supported limit")]
    TagRegionTooLarge,
    /// The underlying reader failed.
    #[error("the stream could not be read")]
    Io,
}

/// A failed observation: the format it was attempted as, and why it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("could not read {format} metadata: {kind}")]
pub struct ObserveError {
    pub format: AudioFormat,
    pub kind: ObserveErrorKind,
}

impl ObserveError {
    fn new(format: AudioFormat, kind: ObserveErrorKind) -> ObserveError {
        ObserveError { format, kind }
    }
}

impl From<ObserveError> for Error {
    /// Fold an observation failure into the engine's tagging error. Safe by
    /// construction: [`ObserveError`]'s message is fixed text.
    fn from(err: ObserveError) -> Error {
        Error::Tag(err.to_string())
    }
}

/// Observe the metadata `source` currently carries, reading it as `format`.
///
/// Reads metadata and nothing else. FLAC stops after the last metadata block,
/// MP3 buffers only the ID3v2 region its header declares, WAV walks the RIFF
/// chunk headers and seeks over `fmt `/`data` to reach `ID3 `, and MP4 lets
/// `mp4ameta` seek over `mdat` to `moov`. A multi-megabyte file therefore costs
/// a few kilobytes of reads.
///
/// Returns [`TagStatus::Absent`] for a valid file with no metadata region, and
/// an [`ObserveError`] only when the container itself could not be understood.
/// Arbitrary bytes never panic: a third-party parser that panics on malformed
/// input is contained and reported as [`ObserveErrorKind::Malformed`].
///
/// Key normalisation is per format and limited to identifiers the writer owns.
/// Vorbis names are matched case-insensitively, while alternate spellings and
/// legacy ID3 aliases remain foreign so a preserving retag cannot loop on
/// metadata it deliberately leaves untouched.
///
/// # Limitations
///
/// - MP3 observes the leading ID3v2 tag only. An ID3v1 trailer is not read (it
///   would mean seeking to the end of the file for a tag rs-suno never writes),
///   and no MPEG frame is parsed, so a file with neither an ID3v2 header nor
///   audio reads as untagged rather than as malformed.
/// - ALAC is verified as an MP4 container, not as the ALAC codec: proving the
///   codec would mean walking the sample tables.
/// - Entry ordering is not observed. `metaflac` decodes Vorbis comments into a
///   map, so the order they appear in the file is lost before this module sees
///   them; [`entry_count`](ObservedAudio::entry_count) still shows a field
///   arriving or leaving.
pub fn observe<R: AudioSource>(
    format: AudioFormat,
    source: &mut R,
) -> Result<ObservedAudio, ObserveError> {
    // Contain a panic from a third-party parser on malformed input (metaflac
    // 0.2.8 slices unchecked while parsing metadata blocks, as tag_flac already
    // documents) so an observation of a corrupt file returns an error rather
    // than unwinding through the run. AssertUnwindSafe is sound: the closure
    // borrows only the source, which an unwind cannot leave observably broken
    // because a failed observation discards it.
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match format {
        AudioFormat::Flac => observe_flac(source),
        AudioFormat::Mp3 => observe_mp3(source),
        AudioFormat::Wav => observe_wav(source),
        AudioFormat::Alac => observe_alac(source),
    }));
    match observed {
        Ok(observed) => observed,
        Err(_) => Err(ObserveError::new(format, ObserveErrorKind::Malformed)),
    }
}

/// Observe an in-memory file. Convenience over [`observe`] for a caller that
/// already holds the bytes, above all a test.
pub fn observe_bytes(format: AudioFormat, bytes: &[u8]) -> Result<ObservedAudio, ObserveError> {
    observe(format, &mut Cursor::new(bytes))
}

/// A stable digest of an embedded image.
///
/// Matches [`ObservedCover::fingerprint`], so a caller can compare the cover it
/// is about to embed against the one already in the file without decoding
/// either. FNV-1a, as [`content_hash`](crate::content_hash) uses for text, so
/// the digest is stable across runs, versions, and platforms.
pub fn cover_fingerprint(bytes: &[u8]) -> String {
    bytes_digest(b"cover", bytes)
}

/// A stable digest of a timed-lyrics entry list.
///
/// Matches [`ObservedTimedLyrics::fingerprint`], so a caller holding the
/// `(timestamp, text)` pairs it would write can tell whether the file already
/// carries exactly that timing. Order is significant here, unlike for managed
/// field values: a reordered lyric line is a different lyric line.
pub fn timed_lyrics_fingerprint(entries: &[(u32, String)]) -> String {
    let mut rendered = String::new();
    for (timestamp, text) in entries {
        rendered.push_str(&format!("{timestamp}\u{1}{text}\u{0}"));
    }
    content_hash(&rendered)
}

/// FNV-1a over a domain separator and bytes, rendered as 16 hex characters.
///
/// The same algorithm and rendering as [`content_hash`](crate::content_hash),
/// which only takes text; the separator keeps digests of different kinds from
/// colliding.
fn bytes_digest(domain: &[u8], bytes: &[u8]) -> String {
    use std::hash::Hasher;

    let mut hasher = fnv::FnvHasher::default();
    hasher.write(domain);
    hasher.write_u8(0);
    hasher.write(bytes);
    format!("{:016x}", hasher.finish())
}

/// Rewind to the start of the stream.
fn rewind<R: AudioSource>(source: &mut R, format: AudioFormat) -> Result<(), ObserveError> {
    source
        .rewind()
        .map_err(|_| ObserveError::new(format, ObserveErrorKind::Io))
}

/// Read the leading `magic.len()` bytes, or `None` when the stream is shorter.
fn read_magic<R: AudioSource>(
    source: &mut R,
    magic: &mut [u8],
    format: AudioFormat,
) -> Result<bool, ObserveError> {
    rewind(source, format)?;
    match source.read_exact(magic) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(_) => Err(ObserveError::new(format, ObserveErrorKind::Io)),
    }
}

/// Buffer `len` bytes of metadata region, refusing an implausible length.
///
/// The length always comes from the file itself, so it is checked twice before
/// anything is allocated: against a fixed ceiling, and against what actually
/// remains in the stream. A corrupt header can therefore neither provoke a large
/// allocation nor be read past the end of the file.
fn read_region<R: AudioSource>(
    source: &mut R,
    len: u64,
    format: AudioFormat,
) -> Result<Vec<u8>, ObserveError> {
    if len > MAX_TAG_REGION_BYTES {
        return Err(ObserveError::new(
            format,
            ObserveErrorKind::TagRegionTooLarge,
        ));
    }
    let position = source
        .stream_position()
        .map_err(|_| ObserveError::new(format, ObserveErrorKind::Io))?;
    let end = stream_len(source, format)?;
    if end.saturating_sub(position) < len {
        return Err(ObserveError::new(format, ObserveErrorKind::Malformed));
    }
    let size = usize::try_from(len)
        .map_err(|_| ObserveError::new(format, ObserveErrorKind::TagRegionTooLarge))?;
    let mut region = vec![0u8; size];
    source
        .read_exact(&mut region)
        .map_err(|_| ObserveError::new(format, ObserveErrorKind::Malformed))?;
    Ok(region)
}

/// The stream's total length, leaving the position where it was. Two seeks and
/// no reads.
fn stream_len<R: AudioSource>(source: &mut R, format: AudioFormat) -> Result<u64, ObserveError> {
    let io = |_| ObserveError::new(format, ObserveErrorKind::Io);
    let position = source.stream_position().map_err(io)?;
    let end = source.seek(SeekFrom::End(0)).map_err(io)?;
    source.seek(SeekFrom::Start(position)).map_err(io)?;
    Ok(end)
}

/// Record a `TRACKNUMBER`-shaped value, splitting the `number/total` spelling.
fn add_track_value(tags: &mut ManagedTags, value: &str) {
    match value.split_once('/') {
        Some((number, total)) => {
            add_normalised_number(tags, ManagedField::TrackNumber, number);
            add_normalised_number(tags, ManagedField::TrackTotal, total);
        }
        None => add_normalised_number(tags, ManagedField::TrackNumber, value),
    }
}

/// Record a numeric field, canonicalising `"07"` to `"7"` and keeping a
/// non-numeric value verbatim rather than dropping it.
fn add_normalised_number(tags: &mut ManagedTags, field: ManagedField, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    match trimmed.parse::<u32>() {
        Ok(number) => tags.add(field, number.to_string()),
        Err(_) => tags.add(field, trimmed),
    }
}

/// Sort foreign entries into their canonical order so two observations of the
/// same file fingerprint identically.
fn sort_foreign(foreign: &mut [ForeignEntry]) {
    foreign.sort_by(|left, right| {
        left.key
            .cmp(&right.key)
            .then_with(|| foreign_order_key(&left.value).cmp(&foreign_order_key(&right.value)))
    });
}

/// A total, deterministic order over foreign values.
fn foreign_order_key(value: &ForeignValue) -> String {
    match value {
        ForeignValue::Text(text) => format!("0{text}"),
        ForeignValue::Opaque { len, fingerprint } => format!("1{len}:{fingerprint}"),
        ForeignValue::Present => "2".to_owned(),
    }
}

/// Walk the FLAC metadata block headers, returning where the metadata ends.
///
/// Four bytes are read per block header and the body is seeked over, so the walk
/// costs a few dozen bytes and never touches an audio frame. A declared length
/// that runs past the end of the file, a run of blocks that never sets the
/// last-block flag, or a metadata region beyond the ceiling is malformed: the
/// underlying decoder is lenient about all three and would otherwise report a
/// truncated file as a valid untagged one.
fn validate_flac_blocks<R: AudioSource>(source: &mut R) -> Result<u64, ObserveError> {
    const FORMAT: AudioFormat = AudioFormat::Flac;

    let malformed = || ObserveError::new(FORMAT, ObserveErrorKind::Malformed);
    let end = stream_len(source, FORMAT)?;
    let mut position = source
        .seek(SeekFrom::Start(4))
        .map_err(|_| ObserveError::new(FORMAT, ObserveErrorKind::Io))?;

    for _ in 0..MAX_FLAC_BLOCKS {
        let mut header = [0u8; 4];
        source.read_exact(&mut header).map_err(|_| malformed())?;
        let is_last = header[0] & 0x80 != 0;
        let length = u64::from(u32::from_be_bytes([0, header[1], header[2], header[3]]));
        position = position
            .checked_add(4)
            .and_then(|start| start.checked_add(length))
            .ok_or_else(malformed)?;
        if position > end {
            return Err(malformed());
        }
        if position > MAX_TAG_REGION_BYTES {
            return Err(ObserveError::new(
                FORMAT,
                ObserveErrorKind::TagRegionTooLarge,
            ));
        }
        if is_last {
            return Ok(position);
        }
        source
            .seek(SeekFrom::Start(position))
            .map_err(|_| ObserveError::new(FORMAT, ObserveErrorKind::Io))?;
    }
    Err(malformed())
}

/// Observe a FLAC: metadata blocks only, stopping at the last-block flag.
fn observe_flac<R: AudioSource>(source: &mut R) -> Result<ObservedAudio, ObserveError> {
    const FORMAT: AudioFormat = AudioFormat::Flac;

    let mut magic = [0u8; 4];
    if !read_magic(source, &mut magic, FORMAT)? || &magic != b"fLaC" {
        return Err(ObserveError::new(FORMAT, ObserveErrorKind::NotThisFormat));
    }
    rewind(source, FORMAT)?;
    // Walk the block headers first, with seeks rather than reads, so a length
    // that overruns the file is reported as malformed. metaflac is lenient
    // here: it hands back whatever it managed to read, which would otherwise
    // make a truncated file look like a valid untagged one.
    validate_flac_blocks(source)?;
    rewind(source, FORMAT)?;
    // metaflac reads the `fLaC` marker and then each metadata block in turn,
    // stopping at the one whose last-block flag is set: the audio frames that
    // follow are never touched.
    let tag = metaflac::Tag::read_from(source)
        .map_err(|_| ObserveError::new(FORMAT, ObserveErrorKind::Malformed))?;

    let mut observed = ObservedAudio::untagged(FORMAT);
    for block in tag.blocks() {
        match block {
            metaflac::Block::StreamInfo(info) => {
                observed.audio_signature = md5_hex(&info.md5);
            }
            metaflac::Block::VorbisComment(comments) => {
                observed.status = TagStatus::Present;
                if !comments.vendor_string.is_empty() {
                    observed.foreign.push(ForeignEntry {
                        key: "VORBIS_VENDOR".to_owned(),
                        value: ForeignValue::Text(comments.vendor_string.clone()),
                    });
                }
                let mut keys: Vec<&String> = comments.comments.keys().collect();
                keys.sort();
                for key in keys {
                    let Some(values) = comments.comments.get(key) else {
                        continue;
                    };
                    let key = key.to_uppercase();
                    observed.entry_count += values.len();
                    for value in values {
                        match ManagedField::from_vorbis(&key) {
                            Some(ManagedField::TrackNumber) => {
                                add_track_value(&mut observed.managed, value);
                            }
                            Some(field @ ManagedField::TrackTotal) => {
                                add_normalised_number(&mut observed.managed, field, value);
                            }
                            Some(field) => observed.managed.add(field, value.clone()),
                            None => observed.foreign.push(ForeignEntry {
                                key: key.clone(),
                                value: ForeignValue::Text(value.clone()),
                            }),
                        }
                    }
                }
            }
            metaflac::Block::Picture(picture) => {
                observed.status = TagStatus::Present;
                observed.entry_count += 1;
                let is_front = picture.picture_type == metaflac::block::PictureType::CoverFront;
                let cover = ObservedCover {
                    mime: picture.mime_type.to_lowercase(),
                    description: picture.description.clone(),
                    len: picture.data.len(),
                    fingerprint: cover_fingerprint(&picture.data),
                };
                if is_front && observed.cover.is_none() {
                    observed.cover = Some(cover);
                } else if picture.picture_type == metaflac::block::PictureType::Other
                    && picture.description == STATIC_FALLBACK_DESCRIPTION
                    && observed.static_fallback.is_none()
                {
                    observed.static_fallback = Some(cover);
                } else {
                    observed.foreign.push(ForeignEntry {
                        key: format!("PICTURE:{:?}", picture.picture_type),
                        value: ForeignValue::Opaque {
                            len: picture.data.len(),
                            fingerprint: cover_fingerprint(&picture.data),
                        },
                    });
                }
            }
            metaflac::Block::Padding(_) => {}
            other => {
                observed.entry_count += 1;
                observed.foreign.push(flac_foreign_block(other));
            }
        }
    }
    sort_foreign(&mut observed.foreign);
    Ok(observed)
}

/// Describe a FLAC metadata block rs-suno does not manage, fingerprinting its
/// serialised bytes so a caller can assert a rewrite preserved it.
fn flac_foreign_block(block: &metaflac::Block) -> ForeignEntry {
    let key = match block {
        metaflac::Block::Application(_) => "BLOCK:APPLICATION".to_owned(),
        metaflac::Block::CueSheet(_) => "BLOCK:CUESHEET".to_owned(),
        metaflac::Block::SeekTable(_) => "BLOCK:SEEKTABLE".to_owned(),
        metaflac::Block::Unknown((kind, _)) => format!("BLOCK:UNKNOWN:{kind}"),
        _ => "BLOCK:OTHER".to_owned(),
    };
    let mut bytes = Vec::new();
    let value = match block.write_to(false, &mut bytes) {
        Ok(_) => ForeignValue::Opaque {
            len: bytes.len(),
            fingerprint: bytes_digest(b"flac-block", &bytes),
        },
        Err(_) => ForeignValue::Present,
    };
    ForeignEntry { key, value }
}

/// The lower-case hex of a FLAC `STREAMINFO` MD5, or `None` when it is unset.
fn md5_hex(md5: &[u8]) -> Option<String> {
    if md5.len() != 16 || md5.iter().all(|byte| *byte == 0) {
        return None;
    }
    let mut hex = String::with_capacity(32);
    for byte in md5 {
        hex.push_str(&format!("{byte:02x}"));
    }
    Some(hex)
}

/// Observe an MP3: the leading ID3v2 region only.
fn observe_mp3<R: AudioSource>(source: &mut R) -> Result<ObservedAudio, ObserveError> {
    const FORMAT: AudioFormat = AudioFormat::Mp3;

    let mut header = [0u8; ID3_HEADER_LEN];
    if !read_magic(source, &mut header, FORMAT)? || &header[0..3] != b"ID3" {
        // No ID3v2 tag. The MPEG frames are deliberately not parsed, so this is
        // an untagged file rather than a rejected one.
        return Ok(ObservedAudio::untagged(FORMAT));
    }
    let size = syncsafe_len(&header[6..10])
        .ok_or_else(|| ObserveError::new(FORMAT, ObserveErrorKind::Malformed))?;
    // The declared size covers the frames only; the header is already buffered.
    let body = read_region(source, u64::from(size), FORMAT)?;
    let mut region = Vec::with_capacity(ID3_HEADER_LEN + body.len());
    region.extend_from_slice(&header);
    region.extend_from_slice(&body);

    let tag = decode_id3(&region, FORMAT)?;
    Ok(observe_id3(FORMAT, tag.as_ref()))
}

/// The 28-bit value an ID3v2 syncsafe length field holds, or `None` when a byte
/// has its high bit set (which makes the field, and so the tag, malformed).
fn syncsafe_len(bytes: &[u8]) -> Option<u32> {
    let mut size: u32 = 0;
    for byte in bytes {
        if byte & 0x80 != 0 {
            return None;
        }
        size = (size << 7) | u32::from(*byte);
    }
    Some(size)
}

/// Observe a WAV: walk the RIFF chunk headers, seeking over `fmt `, `data`, and
/// anything else, and decode the `ID3 ` chunk when one is found.
fn observe_wav<R: AudioSource>(source: &mut R) -> Result<ObservedAudio, ObserveError> {
    const FORMAT: AudioFormat = AudioFormat::Wav;

    let mut header = [0u8; 12];
    if !read_magic(source, &mut header, FORMAT)?
        || &header[0..4] != b"RIFF"
        || &header[8..12] != b"WAVE"
    {
        return Err(ObserveError::new(FORMAT, ObserveErrorKind::NotThisFormat));
    }

    for _ in 0..MAX_RIFF_CHUNKS {
        let mut chunk = [0u8; 8];
        match source.read_exact(&mut chunk) {
            Ok(()) => {}
            // A clean end of the chunk sequence: no ID3 chunk in this file.
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(ObservedAudio::untagged(FORMAT));
            }
            Err(_) => return Err(ObserveError::new(FORMAT, ObserveErrorKind::Io)),
        }
        let len = u64::from(u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]));
        if chunk[0..4].eq_ignore_ascii_case(b"id3 ") {
            let region = read_region(source, len, FORMAT)?;
            let tag = decode_id3(&region, FORMAT)?;
            return Ok(observe_id3(FORMAT, tag.as_ref()));
        }
        // Skip the payload (a RIFF chunk is word-aligned, so an odd length is
        // followed by a pad byte). A seek, never a read: the PCM never enters
        // the process.
        let skip = len + (len % 2);
        let skipped = i64::try_from(skip)
            .map_err(|_| ObserveError::new(FORMAT, ObserveErrorKind::Malformed))?;
        if source.seek(SeekFrom::Current(skipped)).is_err() {
            return Err(ObserveError::new(FORMAT, ObserveErrorKind::Io));
        }
    }
    Err(ObserveError::new(FORMAT, ObserveErrorKind::Malformed))
}

/// Decode a buffered ID3 region, mapping "no tag here" to `None`.
fn decode_id3(region: &[u8], format: AudioFormat) -> Result<Option<id3::Tag>, ObserveError> {
    match id3::Tag::read_from2(Cursor::new(region)) {
        Ok(tag) => Ok(Some(tag)),
        Err(err) if matches!(err.kind, id3::ErrorKind::NoTag) => Ok(None),
        Err(_) => Err(ObserveError::new(format, ObserveErrorKind::UnreadableTags)),
    }
}

/// Map a decoded ID3 tag onto the container-neutral observation. Shared by MP3
/// and WAV, which differ only in where the region lives.
fn observe_id3(format: AudioFormat, tag: Option<&id3::Tag>) -> ObservedAudio {
    let mut observed = ObservedAudio::untagged(format);
    let Some(tag) = tag else {
        return observed;
    };
    observed.status = TagStatus::Present;

    for frame in tag.frames() {
        observed.entry_count += 1;
        let id = frame.id();
        match frame.content() {
            Content::Text(text) => id3_text(&mut observed, id, text),
            Content::ExtendedText(extended) => match SunoField::from_key(&extended.description) {
                Some(field) => observed
                    .managed
                    .add(ManagedField::Suno(field), extended.value.clone()),
                None => observed.foreign.push(ForeignEntry {
                    key: format!("TXXX:{}", extended.description),
                    value: ForeignValue::Text(extended.value.clone()),
                }),
            },
            Content::Comment(comment)
                if comment.lang == "eng" && comment.description.is_empty() =>
            {
                observed
                    .managed
                    .add(ManagedField::Description, comment.text.clone());
            }
            Content::Comment(comment) => observed.foreign.push(ForeignEntry {
                key: format!("COMM:{}:{}", comment.lang, comment.description),
                value: ForeignValue::Text(comment.text.clone()),
            }),
            Content::Lyrics(lyrics) if lyrics.lang == "eng" && lyrics.description.is_empty() => {
                observed
                    .managed
                    .add(ManagedField::Lyrics, lyrics.text.clone());
            }
            Content::Lyrics(lyrics) => observed.foreign.push(ForeignEntry {
                key: format!("USLT:{}:{}", lyrics.lang, lyrics.description),
                value: ForeignValue::Text(lyrics.text.clone()),
            }),
            Content::SynchronisedLyrics(synced)
                if synced.lang == "eng"
                    && synced.description.is_empty()
                    && synced.timestamp_format == TimestampFormat::Ms
                    && synced.content_type == SynchronisedLyricsType::Lyrics
                    && observed.timed_lyrics.is_none() =>
            {
                observed.timed_lyrics = Some(observe_sylt(synced));
            }
            Content::SynchronisedLyrics(synced) => observed.foreign.push(ForeignEntry {
                key: format!("SYLT:{}:{}", synced.lang, synced.description),
                value: ForeignValue::Opaque {
                    len: synced.content.len(),
                    fingerprint: timed_lyrics_fingerprint(&synced.content),
                },
            }),
            Content::Picture(picture) => {
                let fingerprint = cover_fingerprint(&picture.data);
                let cover = ObservedCover {
                    mime: picture.mime_type.to_lowercase(),
                    description: picture.description.clone(),
                    len: picture.data.len(),
                    fingerprint,
                };
                if picture.picture_type == PictureType::CoverFront && observed.cover.is_none() {
                    observed.cover = Some(cover);
                } else if picture.picture_type == PictureType::Other
                    && picture.description == STATIC_FALLBACK_DESCRIPTION
                    && observed.static_fallback.is_none()
                {
                    observed.static_fallback = Some(cover);
                } else {
                    observed.foreign.push(ForeignEntry {
                        key: format!("APIC:{:?}", picture.picture_type),
                        value: ForeignValue::Opaque {
                            len: picture.data.len(),
                            fingerprint: cover.fingerprint,
                        },
                    });
                }
            }
            Content::Unknown(unknown) => observed.foreign.push(ForeignEntry {
                key: id.to_owned(),
                value: ForeignValue::Opaque {
                    len: unknown.data.len(),
                    fingerprint: bytes_digest(b"id3-frame", &unknown.data),
                },
            }),
            _ => observed.foreign.push(ForeignEntry {
                key: id.to_owned(),
                value: ForeignValue::Present,
            }),
        }
    }
    sort_foreign(&mut observed.foreign);
    observed
}

/// Record an ID3 text frame under its managed field, or as foreign.
///
/// An ID3v2.4 text frame may hold several values separated by a null, so the
/// value is split rather than kept as one string with an embedded null.
fn id3_text(observed: &mut ObservedAudio, id: &str, text: &str) {
    let field = match id {
        "TIT2" => ManagedField::Title,
        "TPE1" => ManagedField::Artist,
        "TALB" => ManagedField::Album,
        "TPE2" => ManagedField::AlbumArtist,
        "TDRC" => ManagedField::Date,
        "TDRL" => ManagedField::Year,
        "TRCK" => {
            for value in text.split('\u{0}').filter(|value| !value.is_empty()) {
                add_track_value(&mut observed.managed, value);
            }
            return;
        }
        _ => {
            observed.foreign.push(ForeignEntry {
                key: id.to_owned(),
                value: ForeignValue::Text(text.to_owned()),
            });
            return;
        }
    };
    let mut values = text.split('\u{0}').peekable();
    while let Some(value) = values.next() {
        // A trailing null is a terminator, not an empty value; a lone empty
        // string is still recorded, so an empty tag stays visible.
        if value.is_empty() && values.peek().is_some() {
            continue;
        }
        observed.managed.add(field, value.to_owned());
    }
}

/// Summarise an ID3 `SYLT` frame.
fn observe_sylt(synced: &id3::frame::SynchronisedLyrics) -> ObservedTimedLyrics {
    let representation = match synced.timestamp_format {
        TimestampFormat::Ms => TimedLyricsRepresentation::Id3SyltMilliseconds,
        TimestampFormat::Mpeg => TimedLyricsRepresentation::Id3SyltMpegFrames,
    };
    ObservedTimedLyrics {
        representation,
        language: synced.lang.clone(),
        description: synced.description.clone(),
        entries: synced.content.len(),
        first_timestamp: synced.content.first().map(|entry| entry.0),
        last_timestamp: synced.content.last().map(|entry| entry.0),
        fingerprint: timed_lyrics_fingerprint(&synced.content),
    }
}

/// Observe an ALAC/MP4: seek through the atom tree to `moov`, never into
/// `mdat`.
fn observe_alac<R: AudioSource>(source: &mut R) -> Result<ObservedAudio, ObserveError> {
    const FORMAT: AudioFormat = AudioFormat::Alac;

    let mut header = [0u8; 8];
    if !read_magic(source, &mut header, FORMAT)? || &header[4..8] != b"ftyp" {
        return Err(ObserveError::new(FORMAT, ObserveErrorKind::NotThisFormat));
    }
    rewind(source, FORMAT)?;
    // Metadata items and artwork only: chapters and audio info would mean
    // parsing the sample tables, which the observation has no use for.
    let config = mp4ameta::ReadConfig {
        read_meta_items: true,
        read_image_data: true,
        ..mp4ameta::ReadConfig::NONE
    };
    let tag = mp4ameta::Tag::read_with(source, &config)
        .map_err(|_| ObserveError::new(FORMAT, ObserveErrorKind::Malformed))?;

    let mut observed = ObservedAudio::untagged(FORMAT);
    let (track, track_total) = tag.track();
    if let Some(track) = track {
        observed.managed.add(ManagedField::TrackNumber, {
            let value: u32 = track.into();
            value.to_string()
        });
    }
    if let Some(total) = track_total {
        observed.managed.add(ManagedField::TrackTotal, {
            let value: u32 = total.into();
            value.to_string()
        });
    }

    for (ident, data) in tag.data() {
        observed.entry_count += 1;
        let key = mp4_key(ident);
        if key == "trkn" {
            // Already read through the typed accessor, which understands the
            // packed number/total layout.
            continue;
        }
        let field = mp4_managed_field(ident);
        match (field, data) {
            (
                _,
                mp4ameta::Data::Jpeg(bytes)
                | mp4ameta::Data::Png(bytes)
                | mp4ameta::Data::Bmp(bytes),
            ) => {
                let cover = ObservedCover {
                    mime: mp4_image_mime(data).to_owned(),
                    description: String::new(),
                    len: bytes.len(),
                    fingerprint: cover_fingerprint(bytes),
                };
                if key == "covr" && observed.cover.is_none() {
                    observed.cover = Some(cover);
                } else {
                    observed.foreign.push(ForeignEntry {
                        key,
                        value: ForeignValue::Opaque {
                            len: cover.len,
                            fingerprint: cover.fingerprint,
                        },
                    });
                }
            }
            (Some(field), mp4ameta::Data::Utf8(text) | mp4ameta::Data::Utf16(text)) => {
                observed.managed.add(field, text.clone());
            }
            (None, mp4ameta::Data::Utf8(text) | mp4ameta::Data::Utf16(text)) => {
                observed.foreign.push(ForeignEntry {
                    key,
                    value: ForeignValue::Text(text.clone()),
                });
            }
            (
                _,
                mp4ameta::Data::Reserved(bytes)
                | mp4ameta::Data::BeSigned(bytes)
                | mp4ameta::Data::Unknown { data: bytes, .. },
            ) => observed.foreign.push(ForeignEntry {
                key,
                value: ForeignValue::Opaque {
                    len: bytes.len(),
                    fingerprint: bytes_digest(b"mp4-atom", bytes),
                },
            }),
        }
    }
    if observed.entry_count > 0 {
        observed.status = TagStatus::Present;
    }
    sort_foreign(&mut observed.foreign);
    Ok(observed)
}

/// The native key an MP4 metadata item is stored under.
fn mp4_key(ident: &mp4ameta::DataIdent) -> String {
    match ident {
        mp4ameta::DataIdent::Fourcc(fourcc) => {
            String::from_utf8_lossy(fourcc.as_slice()).into_owned()
        }
        mp4ameta::DataIdent::Freeform { mean, name } => format!("----:{mean}:{name}"),
    }
}

/// The managed field an MP4 item maps to, or `None` when it is foreign.
fn mp4_managed_field(ident: &mp4ameta::DataIdent) -> Option<ManagedField> {
    match ident {
        mp4ameta::DataIdent::Fourcc(fourcc) => match fourcc.as_slice() {
            b"\xa9nam" => Some(ManagedField::Title),
            b"\xa9ART" => Some(ManagedField::Artist),
            b"\xa9alb" => Some(ManagedField::Album),
            b"aART" => Some(ManagedField::AlbumArtist),
            b"\xa9day" => Some(ManagedField::Year),
            b"\xa9cmt" => Some(ManagedField::Description),
            b"\xa9lyr" => Some(ManagedField::Lyrics),
            _ => None,
        },
        mp4ameta::DataIdent::Freeform { mean, name } if mean == APPLE_ITUNES_MEAN => {
            match name.as_ref() {
                "DATE" => Some(ManagedField::Date),
                key => SunoField::from_key(key).map(ManagedField::Suno),
            }
        }
        mp4ameta::DataIdent::Freeform { .. } => None,
    }
}

/// The MIME type an MP4 artwork data type stands for.
fn mp4_image_mime(data: &mp4ameta::Data) -> &'static str {
    match data {
        mp4ameta::Data::Png(_) => "image/png",
        mp4ameta::Data::Bmp(_) => "image/bmp",
        _ => "image/jpeg",
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::ops::Range;

    use id3::TagLike;
    use proptest::prelude::*;

    use super::*;
    use crate::lyrics::{AlignedLine, AlignedLineWord, AlignedLyrics};
    use crate::tag::{Cover, TrackMetadata, tag_flac, tag_mp3, tag_wav};
    use crate::tag_alac::tag_alac;

    /// A stand-in audio payload big enough that reading it would be obvious in
    /// the byte counts, filled with a pattern no header could be mistaken for.
    const PAYLOAD_LEN: usize = 256 * 1024;

    /// A `Read + Seek` source that records every byte range it is asked to read,
    /// so a test can prove which parts of a file a parser touched. Seeks are
    /// free: skipping over a payload is exactly what the parsers must do.
    struct CountingSource {
        inner: Cursor<Vec<u8>>,
        reads: Vec<Range<u64>>,
    }

    impl CountingSource {
        fn new(bytes: Vec<u8>) -> CountingSource {
            CountingSource {
                inner: Cursor::new(bytes),
                reads: Vec::new(),
            }
        }

        /// Total bytes handed to the parser.
        fn bytes_read(&self) -> u64 {
            self.reads.iter().map(|range| range.end - range.start).sum()
        }

        /// Whether any read overlapped `range`.
        fn touched(&self, range: &Range<u64>) -> bool {
            self.reads
                .iter()
                .any(|read| read.start < range.end && range.start < read.end)
        }

        /// The highest offset any read reached.
        fn max_offset(&self) -> u64 {
            self.reads.iter().map(|range| range.end).max().unwrap_or(0)
        }
    }

    impl Read for CountingSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let start = self.inner.position();
            let read = self.inner.read(buf)?;
            if read > 0 {
                self.reads.push(start..start + read as u64);
            }
            Ok(read)
        }
    }

    impl Seek for CountingSource {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// A representative tag set: every standard field, every Suno field, a
    /// track pair, and lyrics.
    fn sample_meta() -> TrackMetadata {
        TrackMetadata {
            title: "Electric Storm".to_owned(),
            artist: "Alice".to_owned(),
            album: "Weather Series".to_owned(),
            album_artist: "Alice".to_owned(),
            date: "2026-07-05".to_owned(),
            year: "2026".to_owned(),
            lyrics: "thunder rolls\nover the plains".to_owned(),
            prompt: "an orchestral storm".to_owned(),
            comment: "stormy".to_owned(),
            style: "ambient, cinematic".to_owned(),
            style_summary: "stormy".to_owned(),
            model: "chirp (v4)".to_owned(),
            handle: "alice".to_owned(),
            parent: "parent-id".to_owned(),
            root: "root-id".to_owned(),
            lineage: "Extension of parent-i".to_owned(),
            id: "clip-id".to_owned(),
            url: "https://suno.com/song/clip-id".to_owned(),
            track: 3,
            track_total: 9,
        }
    }

    fn sample_cover() -> Vec<u8> {
        b"\xFF\xD8\xFF\xE0jpeg-cover-bytes".to_vec()
    }

    fn sample_aligned() -> AlignedLyrics {
        AlignedLyrics {
            lines: vec![
                AlignedLine {
                    text: "thunder rolls".to_owned(),
                    start_s: 0.5,
                    end_s: 1.4,
                    section: "Verse 1".to_owned(),
                    words: vec![AlignedLineWord {
                        text: "thunder".to_owned(),
                        start_s: 0.5,
                        end_s: 0.9,
                    }],
                },
                AlignedLine {
                    text: "over the plains".to_owned(),
                    start_s: 61.2,
                    end_s: 61.8,
                    section: "Chorus".to_owned(),
                    words: vec![AlignedLineWord {
                        text: "over".to_owned(),
                        start_s: 61.2,
                        end_s: 61.8,
                    }],
                },
            ],
            ..Default::default()
        }
    }

    /// A payload of the given length, filled with a repeating pattern that no
    /// container header could be confused for.
    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8 + 3).collect()
    }

    /// A minimal but structurally valid FLAC: signature, STREAMINFO (carrying a
    /// known decoded-audio MD5), then stand-in audio frames.
    fn minimal_flac(frames: &[u8]) -> Vec<u8> {
        let mut streaminfo = vec![0u8; 34];
        streaminfo[0..2].copy_from_slice(&4096u16.to_be_bytes());
        streaminfo[2..4].copy_from_slice(&4096u16.to_be_bytes());
        let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 44_100;
        streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());
        streaminfo[18..34].copy_from_slice(&[0xAB; 16]);

        let mut out = Vec::new();
        out.extend_from_slice(b"fLaC");
        out.push(0x80);
        out.extend_from_slice(&[0x00, 0x00, 0x22]);
        out.extend_from_slice(&streaminfo);
        out.extend_from_slice(frames);
        out
    }

    /// A minimal RIFF/WAVE container with a PCM `fmt ` chunk and a `data` chunk.
    fn minimal_wav(samples: &[u8]) -> Vec<u8> {
        let audio_len = u32::try_from(samples.len()).expect("fixture fits a u32");
        let riff_size = 4u32 + 8 + 16 + 8 + audio_len;

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&44_100u32.to_le_bytes());
        out.extend_from_slice(&88_200u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&audio_len.to_le_bytes());
        out.extend_from_slice(samples);
        out
    }

    fn mp4_atom(fourcc: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let size = u32::try_from(8 + content.len()).expect("fixture fits a u32");
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(fourcc);
        out.extend_from_slice(content);
        out
    }

    /// A version-0 `mvhd`, the one atom `mp4ameta` insists on.
    fn mp4_mvhd() -> Vec<u8> {
        let mut content = Vec::new();
        content.extend_from_slice(&[0, 0, 0, 0]);
        content.extend_from_slice(&0u32.to_be_bytes());
        content.extend_from_slice(&0u32.to_be_bytes());
        content.extend_from_slice(&1000u32.to_be_bytes());
        content.extend_from_slice(&1000u32.to_be_bytes());
        content.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        content.extend_from_slice(&0x0100u16.to_be_bytes());
        content.extend_from_slice(&[0u8; 10]);
        content.extend_from_slice(&[0u8; 36]);
        content.extend_from_slice(&[0u8; 24]);
        content.extend_from_slice(&2u32.to_be_bytes());
        mp4_atom(b"mvhd", &content)
    }

    /// A minimal MP4 with the media payload ahead of the movie box, so a parser
    /// must seek over `mdat` to reach the metadata at all.
    fn minimal_mp4(media: &[u8]) -> Vec<u8> {
        let mut ftyp = Vec::new();
        ftyp.extend_from_slice(b"M4A ");
        ftyp.extend_from_slice(&0u32.to_be_bytes());
        ftyp.extend_from_slice(b"M4A mp42isom");

        let mut out = mp4_atom(b"ftyp", &ftyp);
        out.extend_from_slice(&mp4_atom(b"mdat", media));
        out.extend_from_slice(&mp4_atom(b"moov", &mp4_mvhd()));
        out
    }

    /// The byte range a top-level MP4 atom's payload occupies.
    fn mp4_payload_range(bytes: &[u8], fourcc: &[u8; 4]) -> Range<u64> {
        let mut offset = 0usize;
        while offset + 8 <= bytes.len() {
            let size = u32::from_be_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]) as usize;
            if &bytes[offset + 4..offset + 8] == fourcc {
                return (offset + 8) as u64..(offset + size) as u64;
            }
            if size < 8 {
                break;
            }
            offset += size;
        }
        panic!("fixture must contain the atom");
    }

    /// The byte range a RIFF chunk's payload occupies.
    fn riff_payload_range(bytes: &[u8], id: &[u8; 4]) -> Range<u64> {
        let mut offset = 12usize;
        while offset + 8 <= bytes.len() {
            let size = u32::from_le_bytes([
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]) as usize;
            if &bytes[offset..offset + 4] == id {
                return (offset + 8) as u64..(offset + 8 + size) as u64;
            }
            offset += 8 + size + (size % 2);
        }
        panic!("fixture must contain the chunk");
    }

    fn tagged_flac() -> Vec<u8> {
        let cover = sample_cover();
        tag_flac(
            &minimal_flac(&payload(PAYLOAD_LEN)),
            &sample_meta(),
            Some(Cover::jpeg(&cover)),
        )
        .expect("fixture tags")
    }

    fn tagged_mp3() -> Vec<u8> {
        let cover = sample_cover();
        let aligned = sample_aligned();
        tag_mp3(
            &payload(PAYLOAD_LEN),
            &sample_meta(),
            Some(Cover::jpeg(&cover)),
            Some(&aligned),
        )
        .expect("fixture tags")
    }

    fn tagged_wav() -> Vec<u8> {
        let cover = sample_cover();
        let aligned = sample_aligned();
        tag_wav(
            &minimal_wav(&payload(PAYLOAD_LEN)),
            &sample_meta(),
            Some(Cover::jpeg(&cover)),
            Some(&aligned),
        )
        .expect("fixture tags")
    }

    fn tagged_mp4() -> Vec<u8> {
        let cover = sample_cover();
        tag_alac(
            &minimal_mp4(&payload(PAYLOAD_LEN)),
            &sample_meta(),
            Some(Cover::jpeg(&cover)),
        )
        .expect("fixture tags")
    }

    /// Assert every standard and Suno field survived the round trip through a
    /// container, whatever that container spells them.
    fn assert_sample_fields(observed: &ObservedAudio) {
        let meta = sample_meta();
        assert_eq!(
            observed.first(ManagedField::Title),
            Some(meta.title.as_str())
        );
        assert_eq!(
            observed.first(ManagedField::Artist),
            Some(meta.artist.as_str())
        );
        assert_eq!(
            observed.first(ManagedField::Album),
            Some(meta.album.as_str())
        );
        assert_eq!(
            observed.first(ManagedField::AlbumArtist),
            Some(meta.album_artist.as_str())
        );
        assert_eq!(observed.first(ManagedField::Year), Some(meta.year.as_str()));
        assert_eq!(
            observed.first(ManagedField::Description),
            Some(meta.comment.as_str())
        );
        assert_eq!(observed.lyrics(), Some(meta.lyrics.as_str()));
        assert_eq!(observed.track(), Some(3));
        assert_eq!(observed.track_total(), Some(9));
        assert_eq!(
            observed.first(ManagedField::Suno(SunoField::Prompt)),
            Some(meta.prompt.as_str())
        );
        assert_eq!(
            observed.first(ManagedField::Suno(SunoField::Style)),
            Some(meta.style.as_str())
        );
        assert_eq!(
            observed.first(ManagedField::Suno(SunoField::Model)),
            Some(meta.model.as_str())
        );
        assert_eq!(
            observed.first(ManagedField::Suno(SunoField::Id)),
            Some(meta.id.as_str())
        );
        assert_eq!(
            observed.first(ManagedField::Suno(SunoField::Url)),
            Some(meta.url.as_str())
        );
        assert_eq!(observed.status, TagStatus::Present);
        let cover = observed.cover.as_ref().expect("a cover was embedded");
        assert_eq!(cover.mime, "image/jpeg");
        assert_eq!(cover.fingerprint, cover_fingerprint(&sample_cover()));
        assert_eq!(cover.len, sample_cover().len());
    }

    #[test]
    fn flac_observes_every_written_field() {
        let observed = observe_bytes(AudioFormat::Flac, &tagged_flac()).expect("a valid FLAC");
        assert_sample_fields(&observed);
        // The precise DATE is a Vorbis comment of its own, unlike ID3/MP4.
        assert_eq!(observed.first(ManagedField::Date), Some("2026-07-05"));
        // STREAMINFO records the decoded-audio MD5, so a caller can assert a
        // rewrite left the audio alone without decoding it.
        assert_eq!(
            observed.audio_signature.as_deref(),
            Some(&"ab".repeat(16)[..])
        );
    }

    #[test]
    fn flac_reads_metadata_blocks_only() {
        let bytes = tagged_flac();
        let frames = payload(PAYLOAD_LEN);
        let audio = (bytes.len() - frames.len()) as u64..bytes.len() as u64;
        assert_eq!(&bytes[audio.start as usize..], &frames[..]);

        let mut source = CountingSource::new(bytes);
        let observed = observe(AudioFormat::Flac, &mut source).expect("a valid FLAC");
        assert!(observed.is_tagged());
        assert!(
            !source.touched(&audio),
            "FLAC observation must stop at the last metadata block, read {:?}",
            source.reads
        );
        assert!(
            source.max_offset() <= audio.start,
            "read up to offset {}, past the {} bytes of metadata",
            source.max_offset(),
            audio.start
        );
        // The magic and the block headers are re-read by the structural walk, so
        // the total is a little over the region rather than under it.
        assert!(
            source.bytes_read() <= audio.start + 64,
            "read {} bytes for {} bytes of metadata",
            source.bytes_read(),
            audio.start
        );
    }

    #[test]
    fn flac_without_comments_is_untagged_not_an_error() {
        let observed =
            observe_bytes(AudioFormat::Flac, &minimal_flac(b"\xFF\xF8frames")).expect("valid FLAC");
        assert_eq!(observed.status, TagStatus::Absent);
        assert!(observed.managed.is_empty());
        assert_eq!(observed.entry_count, 0);
        assert!(observed.audio_signature.is_some());
    }

    #[test]
    fn flac_keeps_unmanaged_comments_as_foreign() {
        let mut tag =
            metaflac::Tag::read_from(&mut Cursor::new(tagged_flac())).expect("the fixture parses");
        tag.set_vorbis("REPLAYGAIN_TRACK_GAIN", vec!["-3.21 dB"]);
        tag.vorbis_comments_mut().vendor_string = "reference libFLAC 1.4.3".to_owned();
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes).expect("the fixture rewrites");

        let observed = observe_bytes(AudioFormat::Flac, &bytes).expect("a valid FLAC");
        assert!(
            observed
                .foreign
                .iter()
                .any(|entry| entry.key == "REPLAYGAIN_TRACK_GAIN"
                    && entry.value == ForeignValue::Text("-3.21 dB".to_owned())),
            "an unmanaged comment must be preserved for a preservation assertion: {:?}",
            observed.foreign
        );
        // The vendor string is unmanaged metadata too, and moves when a
        // different writer touches the file.
        assert!(
            observed
                .foreign
                .iter()
                .any(|entry| entry.key == "VORBIS_VENDOR"
                    && entry.value == ForeignValue::Text("reference libFLAC 1.4.3".to_owned()))
        );
    }

    #[test]
    fn flac_normalises_owned_keys_but_keeps_aliases_foreign() {
        let mut tag =
            metaflac::Tag::read_from(&mut Cursor::new(minimal_flac(b"\xFF\xF8frames"))).unwrap();
        tag.set_vorbis("title", vec!["lower cased key"]);
        tag.set_vorbis("TrackNumber", vec!["07/12"]);
        tag.set_vorbis("Album Artist", vec!["Alice"]);
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes).unwrap();

        let observed = observe_bytes(AudioFormat::Flac, &bytes).expect("a valid FLAC");
        assert_eq!(observed.first(ManagedField::Title), Some("lower cased key"));
        assert_eq!(observed.track(), Some(7));
        assert_eq!(observed.track_total(), Some(12));
        assert_eq!(observed.first(ManagedField::AlbumArtist), None);
        assert!(observed.foreign.iter().any(|entry| {
            entry.key == "ALBUM ARTIST" && entry.value == ForeignValue::Text("Alice".to_owned())
        }));
    }

    #[test]
    fn mp3_observes_every_written_field() {
        let observed = observe_bytes(AudioFormat::Mp3, &tagged_mp3()).expect("a valid MP3");
        assert_sample_fields(&observed);
        assert_eq!(observed.first(ManagedField::Date), Some("2026-07-05"));
    }

    #[test]
    fn mp3_reads_only_the_id3_region() {
        let bytes = tagged_mp3();
        let frames = payload(PAYLOAD_LEN);
        let audio = (bytes.len() - frames.len()) as u64..bytes.len() as u64;

        let mut source = CountingSource::new(bytes);
        let observed = observe(AudioFormat::Mp3, &mut source).expect("a valid MP3");
        assert!(observed.is_tagged());
        assert!(
            !source.touched(&audio),
            "MP3 observation must stop at the end of the ID3 region, read {:?}",
            source.reads
        );
        assert_eq!(
            source.bytes_read(),
            audio.start,
            "exactly the declared ID3 region is buffered"
        );
    }

    #[test]
    fn mp3_observes_timed_lyrics_it_can_compare_with_a_planned_write() {
        let observed = observe_bytes(AudioFormat::Mp3, &tagged_mp3()).expect("a valid MP3");
        let timed = observed.timed_lyrics.expect("SYLT was written");
        assert_eq!(
            timed.representation,
            TimedLyricsRepresentation::Id3SyltMilliseconds
        );
        assert_eq!(timed.entries, 2);
        assert_eq!(timed.first_timestamp, Some(500));
        assert_eq!(timed.last_timestamp, Some(61_200));
        assert_eq!(timed.language, "eng");
        // The whole point: the caller can fingerprint what it would write and
        // compare, without re-reading the file.
        let planned = sample_aligned().sylt_entries_with_timing(crate::vocab::LyricsTiming::Line);
        assert_eq!(timed.fingerprint, timed_lyrics_fingerprint(&planned));
    }

    #[test]
    fn mp3_without_an_id3_header_is_untagged() {
        let observed =
            observe_bytes(AudioFormat::Mp3, b"\xFF\xFBno tag here at all").expect("no ID3 tag");
        assert_eq!(observed.status, TagStatus::Absent);
        assert!(!observed.is_tagged());
        assert!(observed.managed.is_empty());
    }

    #[test]
    fn mp3_keeps_unmanaged_frames_as_foreign() {
        let mut tag = id3::Tag::read_from2(Cursor::new(tagged_mp3())).expect("the fixture parses");
        tag.add_frame(id3::frame::ExtendedText {
            description: "REPLAYGAIN_TRACK_GAIN".to_owned(),
            value: "-3.21 dB".to_owned(),
        });
        tag.set_text("TCON", "Ambient");
        tag.set_text("TYER", "1999");
        tag.add_frame(id3::frame::Comment {
            lang: "fra".to_owned(),
            description: String::new(),
            text: "commentaire".to_owned(),
        });
        tag.add_frame(id3::frame::Lyrics {
            lang: "fra".to_owned(),
            description: String::new(),
            text: "paroles".to_owned(),
        });
        let mut bytes = Cursor::new(tagged_mp3());
        tag.write_to_file(&mut bytes, id3::Version::Id3v24)
            .expect("the fixture rewrites");

        let observed = observe_bytes(AudioFormat::Mp3, &bytes.into_inner()).expect("a valid MP3");
        assert!(observed.foreign.iter().any(|entry| {
            entry.key == "TXXX:REPLAYGAIN_TRACK_GAIN"
                && entry.value == ForeignValue::Text("-3.21 dB".to_owned())
        }));
        assert!(
            observed.foreign.iter().any(|entry| entry.key == "TCON"
                && entry.value == ForeignValue::Text("Ambient".to_owned()))
        );
        assert!(observed.foreign.iter().any(|entry| {
            entry.key == "TYER" && entry.value == ForeignValue::Text("1999".to_owned())
        }));
        assert!(observed.foreign.iter().any(|entry| {
            entry.key == "COMM:fra:" && entry.value == ForeignValue::Text("commentaire".to_owned())
        }));
        assert!(observed.foreign.iter().any(|entry| {
            entry.key == "USLT:fra:" && entry.value == ForeignValue::Text("paroles".to_owned())
        }));
    }

    #[test]
    fn wav_observes_every_written_field() {
        let observed = observe_bytes(AudioFormat::Wav, &tagged_wav()).expect("a valid WAV");
        assert_sample_fields(&observed);
        assert!(observed.timed_lyrics.is_some());
    }

    #[test]
    fn wav_seeks_over_the_pcm_data_to_the_id3_chunk() {
        let bytes = tagged_wav();
        let audio = riff_payload_range(&bytes, b"data");
        assert_eq!(audio.end - audio.start, PAYLOAD_LEN as u64);

        let mut source = CountingSource::new(bytes);
        let observed = observe(AudioFormat::Wav, &mut source).expect("a valid WAV");
        assert!(observed.is_tagged());
        assert!(
            !source.touched(&audio),
            "WAV observation must seek over the PCM, read {:?}",
            source.reads
        );
        assert!(
            source.bytes_read() < 64 * 1024,
            "read {} bytes for a 256 KiB file",
            source.bytes_read()
        );
    }

    #[test]
    fn wav_without_an_id3_chunk_is_untagged() {
        let observed =
            observe_bytes(AudioFormat::Wav, &minimal_wav(b"samples")).expect("valid WAV");
        assert_eq!(observed.status, TagStatus::Absent);
        assert!(observed.managed.is_empty());
    }

    #[test]
    fn alac_observes_every_written_field() {
        let observed = observe_bytes(AudioFormat::Alac, &tagged_mp4()).expect("a valid MP4");
        assert_sample_fields(&observed);
        // The MP4 writer keeps the precise date in a freeform atom and the year
        // in `©day`; both normalise onto the same managed fields.
        assert_eq!(observed.first(ManagedField::Date), Some("2026-07-05"));
    }

    #[test]
    fn alac_seeks_over_the_media_payload() {
        let bytes = tagged_mp4();
        let media = mp4_payload_range(&bytes, b"mdat");
        assert_eq!(media.end - media.start, PAYLOAD_LEN as u64);

        let mut source = CountingSource::new(bytes);
        let observed = observe(AudioFormat::Alac, &mut source).expect("a valid MP4");
        assert!(observed.is_tagged());
        assert!(
            !source.touched(&media),
            "MP4 observation must seek over mdat, read {:?}",
            source.reads
        );
        assert!(
            source.bytes_read() < 64 * 1024,
            "read {} bytes for a 256 KiB file",
            source.bytes_read()
        );
    }

    #[test]
    fn alac_without_metadata_items_is_untagged() {
        let observed =
            observe_bytes(AudioFormat::Alac, &minimal_mp4(b"media")).expect("a valid MP4");
        assert_eq!(observed.status, TagStatus::Absent);
        assert!(observed.managed.is_empty());
        assert!(observed.cover.is_none());
    }

    #[test]
    fn a_write_of_the_same_metadata_is_observably_a_no_op() {
        // The planning case: what the writer would produce equals what the file
        // already holds, for every container, so nothing needs rewriting.
        let desired = ManagedTags::from_track_metadata(&sample_meta());
        let policy = ComparePolicy::new(EmptyPolicy::EmptyIsAbsent, DuplicatePolicy::Keep);
        for (format, bytes) in [
            (AudioFormat::Flac, tagged_flac()),
            (AudioFormat::Mp3, tagged_mp3()),
            (AudioFormat::Wav, tagged_wav()),
            (AudioFormat::Alac, tagged_mp4()),
        ] {
            let observed = observe_bytes(format, &bytes).expect("a valid file");
            assert_eq!(
                observed.managed.differences(&desired, policy),
                Vec::new(),
                "{format} observation must match the metadata that produced it"
            );
        }
    }

    #[test]
    fn a_changed_field_is_the_only_reported_difference() {
        let observed = observe_bytes(AudioFormat::Flac, &tagged_flac()).expect("a valid FLAC");
        let mut meta = sample_meta();
        meta.lyrics = "different words".to_owned();
        let desired = ManagedTags::from_track_metadata(&meta);
        assert_eq!(
            observed
                .managed
                .differences(&desired, ComparePolicy::exact()),
            vec![ManagedField::Lyrics]
        );
    }

    #[test]
    fn comparison_ignores_value_order() {
        let mut left = ManagedTags::new();
        left.add(ManagedField::Artist, "Alice");
        left.add(ManagedField::Artist, "Bob");
        let mut right = ManagedTags::new();
        right.add(ManagedField::Artist, "Bob");
        right.add(ManagedField::Artist, "Alice");
        assert!(left.equivalent(&right, ComparePolicy::exact()));
        assert_ne!(
            left, right,
            "order is kept in storage, ignored in comparison"
        );
    }

    #[test]
    fn duplicate_policy_decides_whether_a_repeat_counts() {
        let mut once = ManagedTags::new();
        once.add(ManagedField::Artist, "Alice");
        let mut twice = ManagedTags::new();
        twice.add(ManagedField::Artist, "Alice");
        twice.add(ManagedField::Artist, "Alice");

        assert_eq!(
            once.differences(&twice, ComparePolicy::exact()),
            vec![ManagedField::Artist]
        );
        assert!(once.equivalent(&twice, ComparePolicy::lenient()));
    }

    #[test]
    fn empty_policy_decides_whether_empty_is_absent() {
        let mut empty = ManagedTags::new();
        empty.add(ManagedField::Album, "");
        let absent = ManagedTags::new();

        assert_eq!(
            empty.differences(&absent, ComparePolicy::exact()),
            vec![ManagedField::Album],
            "by default an empty value is a value"
        );
        assert!(
            empty.equivalent(
                &absent,
                ComparePolicy::new(EmptyPolicy::EmptyIsAbsent, DuplicatePolicy::Keep)
            ),
            "and is absent only when the caller says so"
        );
        assert!(empty.contains(ManagedField::Album));
        assert_eq!(empty.first(ManagedField::Album), Some(""));
    }

    #[test]
    fn set_replaces_while_add_appends() {
        let mut tags = ManagedTags::new();
        tags.add(ManagedField::Artist, "Alice");
        tags.add(ManagedField::Artist, "Bob");
        assert_eq!(tags.get(ManagedField::Artist).len(), 2);
        tags.set(ManagedField::Artist, "Carol");
        assert_eq!(tags.get(ManagedField::Artist), ["Carol".to_owned()]);
        assert_eq!(tags.fields().count(), 1);
    }

    #[test]
    fn foreign_fingerprint_tracks_unmanaged_entries() {
        let plain = observe_bytes(AudioFormat::Flac, &tagged_flac()).expect("a valid FLAC");

        let mut tag = metaflac::Tag::read_from(&mut Cursor::new(tagged_flac())).unwrap();
        tag.set_vorbis("ENCODER", vec!["someone else"]);
        let mut bytes = Vec::new();
        tag.write_to(&mut bytes).unwrap();
        let extended = observe_bytes(AudioFormat::Flac, &bytes).expect("a valid FLAC");

        assert_ne!(plain.foreign_fingerprint(), extended.foreign_fingerprint());
        let again = observe_bytes(AudioFormat::Flac, &bytes).expect("a valid FLAC");
        assert_eq!(
            extended.foreign_fingerprint(),
            again.foreign_fingerprint(),
            "the fingerprint is stable across observations"
        );
    }

    #[test]
    fn observation_is_deterministic() {
        for (format, bytes) in [
            (AudioFormat::Flac, tagged_flac()),
            (AudioFormat::Mp3, tagged_mp3()),
            (AudioFormat::Wav, tagged_wav()),
            (AudioFormat::Alac, tagged_mp4()),
        ] {
            let first = observe_bytes(format, &bytes).expect("a valid file");
            let second = observe_bytes(format, &bytes).expect("a valid file");
            assert_eq!(first, second, "{format} observation must not vary");
        }
    }

    #[test]
    fn a_file_read_as_the_wrong_format_is_rejected() {
        for (format, bytes) in [
            (AudioFormat::Flac, tagged_mp3()),
            (AudioFormat::Wav, tagged_flac()),
            (AudioFormat::Alac, tagged_flac()),
        ] {
            let err = observe_bytes(format, &bytes).expect_err("the container does not match");
            assert_eq!(err.kind, ObserveErrorKind::NotThisFormat);
            assert_eq!(err.format, format);
        }
    }

    #[test]
    fn garbage_errors_rather_than_panics_for_every_format() {
        let garbage = b"this is not audio at all, it is prose".to_vec();
        for format in [
            AudioFormat::Flac,
            AudioFormat::Wav,
            AudioFormat::Alac,
            AudioFormat::Mp3,
        ] {
            match observe_bytes(format, &garbage) {
                // MP3 has no header magic beyond the ID3 tag, so garbage with no
                // ID3 header is simply untagged.
                Ok(observed) => assert_eq!(observed.status, TagStatus::Absent),
                Err(err) => assert_eq!(err.format, format),
            }
        }
    }

    #[test]
    fn an_empty_file_never_panics() {
        for format in [
            AudioFormat::Flac,
            AudioFormat::Mp3,
            AudioFormat::Wav,
            AudioFormat::Alac,
        ] {
            let _ = observe_bytes(format, b"");
        }
    }

    #[test]
    fn a_truncated_flac_errors_rather_than_panics() {
        // metaflac 0.2.8 slices unchecked while parsing metadata blocks, so a
        // truncated STREAMINFO panics inside the crate; observation must
        // contain that and report a malformed container.
        let mut truncated = minimal_flac(b"\xFF\xF8frames");
        truncated.truncate(12);
        let err = observe_bytes(AudioFormat::Flac, &truncated).expect_err("a truncated FLAC");
        assert_eq!(err.kind, ObserveErrorKind::Malformed);
    }

    #[test]
    fn a_flac_whose_block_length_overruns_the_file_errors() {
        let mut broken = minimal_flac(b"\xFF\xF8frames");
        // Claim a 16 MiB STREAMINFO in a file of a few dozen bytes.
        broken[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        let observed = observe_bytes(AudioFormat::Flac, &broken);
        assert!(
            observed.is_err(),
            "an overrunning block length must not parse"
        );
    }

    #[test]
    fn an_id3_header_claiming_more_than_the_file_holds_errors() {
        let mut truncated = tagged_mp3();
        truncated.truncate(64);
        let err = observe_bytes(AudioFormat::Mp3, &truncated).expect_err("a truncated ID3 region");
        assert_eq!(err.kind, ObserveErrorKind::Malformed);
    }

    #[test]
    fn an_id3_header_with_a_non_syncsafe_length_errors() {
        let mut broken = tagged_mp3();
        broken[6] = 0xFF;
        let err = observe_bytes(AudioFormat::Mp3, &broken).expect_err("a non-syncsafe length");
        assert_eq!(err.kind, ObserveErrorKind::Malformed);
    }

    #[test]
    fn an_undecodable_id3_region_is_told_apart_from_an_absent_one() {
        let mut broken = tagged_mp3();
        // Keep the header (so a region is located) but corrupt the frames.
        for byte in broken.iter_mut().skip(ID3_HEADER_LEN).take(256) {
            *byte = 0xFF;
        }
        match observe_bytes(AudioFormat::Mp3, &broken) {
            Ok(observed) => assert_eq!(
                observed.status,
                TagStatus::Present,
                "a decodable region is present, however few frames survived"
            ),
            Err(err) => assert_eq!(err.kind, ObserveErrorKind::UnreadableTags),
        }
    }

    #[test]
    fn a_wav_chunk_length_past_the_end_of_the_file_errors_rather_than_looping() {
        let mut broken = tagged_wav();
        let data = riff_payload_range(&broken, b"data");
        let header = data.start as usize - 4;
        broken[header..header + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        // Seeking past the end is legal; the next header read then hits EOF and
        // the walk ends with no ID3 chunk found.
        let observed = observe_bytes(AudioFormat::Wav, &broken).expect("a walkable RIFF");
        assert_eq!(observed.status, TagStatus::Absent);
    }

    #[test]
    fn a_wav_id3_chunk_larger_than_the_file_errors() {
        let mut broken = tagged_wav();
        let id3 = riff_payload_range(&broken, b"ID3 ");
        let header = id3.start as usize - 4;
        broken[header..header + 4].copy_from_slice(&(100 * 1024 * 1024u32).to_le_bytes());
        let err = observe_bytes(AudioFormat::Wav, &broken).expect_err("an implausible chunk");
        assert_eq!(err.kind, ObserveErrorKind::TagRegionTooLarge);
    }

    #[test]
    fn an_mp4_with_no_movie_box_errors() {
        let mut broken = tagged_mp4();
        let moov = mp4_payload_range(&broken, b"moov");
        let header = moov.start as usize - 4;
        broken[header..header + 4].copy_from_slice(b"free");
        let err = observe_bytes(AudioFormat::Alac, &broken).expect_err("no moov atom");
        assert_eq!(err.kind, ObserveErrorKind::Malformed);
    }

    #[test]
    fn errors_never_echo_the_bytes_or_values_they_came_from() {
        // A file whose metadata happens to hold something secret-shaped must not
        // be able to push it into a log through an error message.
        let secret = "sk_live_this_must_never_be_logged";
        let mut meta = sample_meta();
        meta.prompt = secret.to_owned();
        let cover = sample_cover();
        let mut bytes = tag_flac(
            &minimal_flac(b"\xFF\xF8frames"),
            &meta,
            Some(Cover::jpeg(&cover)),
        )
        .unwrap();
        // Corrupt a metadata block header so parsing fails after the tag was
        // built from that metadata.
        bytes[5..8].copy_from_slice(&[0x7F, 0xFF, 0xFF]);

        let err = observe_bytes(AudioFormat::Flac, &bytes).expect_err("a corrupt FLAC");
        let message = err.to_string();
        assert!(
            !message.contains(secret),
            "message leaked a value: {message}"
        );
        assert!(!message.contains("Electric Storm"));
        assert_eq!(
            message,
            "could not read flac metadata: the container structure is malformed"
        );

        let engine: Error = err.into();
        assert!(matches!(engine, Error::Tag(_)));
        assert!(!engine.to_string().contains(secret));
    }

    #[test]
    fn native_keys_name_the_container_spelling() {
        assert_eq!(ManagedField::Title.native_key(AudioFormat::Flac), "TITLE");
        assert_eq!(ManagedField::Title.native_key(AudioFormat::Mp3), "TIT2");
        assert_eq!(ManagedField::Title.native_key(AudioFormat::Wav), "TIT2");
        assert_eq!(
            ManagedField::Title.native_key(AudioFormat::Alac),
            "\u{a9}nam"
        );
        assert_eq!(
            ManagedField::Suno(SunoField::Prompt).native_key(AudioFormat::Mp3),
            "TXXX:SUNO_PROMPT"
        );
        assert_eq!(
            ManagedField::Date.native_key(AudioFormat::Alac),
            "----:com.apple.iTunes:DATE"
        );
        // Every Suno field the writer emits is one this module names.
        for (key, _) in sample_meta().suno_fields() {
            assert!(SunoField::from_key(key).is_some(), "{key} must be managed");
        }
        assert_eq!(SunoField::from_key("SUNO_UNKNOWN"), None);
    }

    #[test]
    fn a_non_numeric_track_value_is_kept_rather_than_dropped() {
        let mut tags = ManagedTags::new();
        add_track_value(&mut tags, "A/B");
        assert_eq!(tags.first(ManagedField::TrackNumber), Some("A"));
        assert_eq!(tags.first(ManagedField::TrackTotal), Some("B"));

        let mut observed = ObservedAudio::untagged(AudioFormat::Flac);
        observed.managed = tags;
        assert_eq!(
            observed.track(),
            None,
            "a non-numeric track reads as absent"
        );
        assert_eq!(observed.track_total(), None);
    }

    #[test]
    fn fingerprints_are_stable_and_domain_separated() {
        assert_eq!(
            cover_fingerprint(b"cover-bytes"),
            cover_fingerprint(b"cover-bytes")
        );
        assert_ne!(cover_fingerprint(b"one"), cover_fingerprint(b"two"));
        assert_eq!(cover_fingerprint(b""), cover_fingerprint(b""));
        assert_ne!(
            cover_fingerprint(b"same"),
            bytes_digest(b"id3-frame", b"same"),
            "different kinds of digest must not collide"
        );

        let entries = vec![(0u32, "one".to_owned()), (10u32, "two".to_owned())];
        let reordered = vec![(10u32, "two".to_owned()), (0u32, "one".to_owned())];
        assert_ne!(
            timed_lyrics_fingerprint(&entries),
            timed_lyrics_fingerprint(&reordered),
            "timing order is significant"
        );
        assert_eq!(timed_lyrics_fingerprint(&[]), timed_lyrics_fingerprint(&[]));
    }

    #[test]
    fn unicode_and_reserved_characters_survive_a_round_trip() {
        let mut meta = sample_meta();
        meta.title = "Ω 桜 🎧 \"quoted\" = sign".to_owned();
        meta.prompt = "line\nbreak\ttab".to_owned();
        let cover = sample_cover();
        let bytes = tag_flac(
            &minimal_flac(b"\xFF\xF8frames"),
            &meta,
            Some(Cover::jpeg(&cover)),
        )
        .expect("unicode tags fine");

        let observed = observe_bytes(AudioFormat::Flac, &bytes).expect("a valid FLAC");
        assert_eq!(
            observed.first(ManagedField::Title),
            Some(meta.title.as_str())
        );
        assert_eq!(
            observed.first(ManagedField::Suno(SunoField::Prompt)),
            Some(meta.prompt.as_str())
        );
        assert!(observed.managed.equivalent(
            &ManagedTags::from_track_metadata(&meta),
            ComparePolicy::lenient()
        ));
    }

    proptest! {
        /// Arbitrary bytes must never panic, whichever format they are read as.
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            for format in [AudioFormat::Flac, AudioFormat::Mp3, AudioFormat::Wav, AudioFormat::Alac] {
                let _ = observe_bytes(format, &bytes);
            }
        }

        /// A RIFF header followed by arbitrary bytes exercises the chunk walk,
        /// which must terminate with an error or an untagged observation rather
        /// than spinning, over-allocating, or panicking.
        #[test]
        fn arbitrary_riff_chunks_never_panic(tail in proptest::collection::vec(any::<u8>(), 0..512)) {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"RIFF");
            bytes.extend_from_slice(&(tail.len() as u32 + 4).to_le_bytes());
            bytes.extend_from_slice(b"WAVE");
            bytes.extend_from_slice(&tail);
            let _ = observe_bytes(AudioFormat::Wav, &bytes);
        }

        /// An ID3 header with an arbitrary declared size and body must never
        /// panic or allocate beyond the region guard.
        #[test]
        fn arbitrary_id3_regions_never_panic(
            size in proptest::collection::vec(0u8..0x80, 4..=4),
            body in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"ID3");
            bytes.extend_from_slice(&[4, 0, 0]);
            bytes.extend_from_slice(&size);
            bytes.extend_from_slice(&body);
            let _ = observe_bytes(AudioFormat::Mp3, &bytes);
        }
    }
}
