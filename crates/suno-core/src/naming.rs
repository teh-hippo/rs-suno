//! Pure naming and relative path rendering for [`Clip`] values.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Clip;
use crate::error::{Error, Result};
use crate::lineage::LineageContext;
use crate::pathkey::canonical_path_key;

/// The default relative path template.
///
/// Supported placeholders are `{creator}`, `{handle}`, `{album}`, `{title}`,
/// `{id}`, `{id8}` (first 8 characters of the clip id), `{root_id8}` (first 8 of
/// the resolved lineage root id), `{track}` (the album track number, e.g. `7`),
/// and `{track2}` (that number zero-padded to two digits, e.g. `07`). An empty
/// placeholder swallows the separator run that follows it, so an unnumbered
/// `{track2}` leaves no orphan ` - `. Empty path segments are dropped after
/// rendering.
///
/// The default prefixes the file name with the two-digit track number, embeds
/// `[{id8}]` so same-title clips never collide, and folders under `{album}`,
/// which resolves to the lineage root's title (else the clip's own title).
pub const DEFAULT_TEMPLATE: &str = "{creator}/{album}/{track2} - {creator}-{title} [{id8}]";
const DEFAULT_MAX_COMPONENT_LEN: usize = 80;

const MIN_BASE_CHARS_WITH_SUFFIX: usize = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum CharacterSet {
    #[default]
    Unicode,
    Ascii,
}

impl FromStr for CharacterSet {
    type Err = Error;

    // Case-sensitive to match serde (TOML) and the JSON schema, which accept
    // lowercase only; the env tier (`SUNO_CHARACTER_SET`) parses through here.
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "unicode" => Ok(Self::Unicode),
            "ascii" => Ok(Self::Ascii),
            other => Err(Error::Config(format!(
                "unknown character_set '{other}'; expected 'unicode' or 'ascii'"
            ))),
        }
    }
}

impl fmt::Display for CharacterSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unicode => f.write_str("unicode"),
            Self::Ascii => f.write_str("ascii"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamingConfig {
    pub template: String,
    pub character_set: CharacterSet,
    pub max_component_len: usize,
}

impl Default for NamingConfig {
    fn default() -> Self {
        Self {
            template: DEFAULT_TEMPLATE.to_string(),
            character_set: CharacterSet::Unicode,
            max_component_len: DEFAULT_MAX_COMPONENT_LEN,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NamingRequest<'a> {
    pub clip: &'a Clip,
    pub lineage: &'a LineageContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedName {
    pub relative_path: PathBuf,
    pub base_name: String,
}

/// Render one clip's relative path in isolation.
///
/// Unlike [`render_clip_names`], this does **not** apply whole-library id
/// disambiguation: a lone clip cannot know whether another clip shares its
/// `{id8}`, so only the plural form (reached via
/// [`build_desired`](crate::desired::build_desired)) appends the stable full-id
/// suffix for an id8-twin. Callers that need that stability must batch through
/// the plural form.
pub fn render_clip_name(request: NamingRequest<'_>, config: &NamingConfig) -> RenderedName {
    let album = album_component(request, config);
    render_with_album(request, config, &album)
}

/// Render every request's relative path as a batch, disambiguating collisions.
///
/// The `{id8}` suffix is **whole-library-stable**: a clip whose id is in
/// `colliding_ids` (the store-derived set of clip ids sharing an `{id8}` with
/// another distinct clip, see
/// [`colliding_clip_ids`](crate::LineageStore::colliding_clip_ids)) gets
/// the full clip id appended regardless of which other clips are in this batch,
/// so trashing or `--limit`-excluding an id8-twin never renames the kept clip
/// (#356). The two trailing batch passes remain as a correctness backstop for
/// template shapes the id8 set cannot see (no `{id8}`/`{id}` placeholder) and for
/// case/NFC collisions.
pub fn render_clip_names(
    requests: &[NamingRequest<'_>],
    config: &NamingConfig,
    colliding_albums: &BTreeSet<String>,
    colliding_ids: &BTreeSet<String>,
) -> Vec<RenderedName> {
    let albums = disambiguated_albums(requests, config, colliding_albums);
    let mut rendered = requests
        .iter()
        .zip(&albums)
        .map(|(request, album)| render_with_album(*request, config, album))
        .collect::<Vec<_>>();

    // Whole-library id pass (#356): append the full clip id to any clip whose
    // id8 is shared archive-wide, before the batch passes so they never re-flag
    // an already-disambiguated pair. The decision depends only on the
    // store-derived set, never on the batch, so it is stable across runs.
    for (index, request) in requests.iter().enumerate() {
        if colliding_ids.contains(&request.clip.id) {
            rendered[index] = with_suffix(
                rendered[index].clone(),
                &request.clip.id,
                config.character_set,
                config.max_component_len,
            );
        }
    }

    // Two passes so distinct clips never land on one path: the first keys on the
    // exact rendered string, the second on the filesystem-canonical form (NFC +
    // lowercase), catching paths that differ only by case or NFD/NFC and would
    // collide on case-insensitive or NFC-normalising filesystems (Windows, macOS).
    for apply_canonical in [false, true] {
        let mut collisions = BTreeMap::<String, Vec<usize>>::new();
        for (index, name) in rendered.iter().enumerate() {
            let key = if apply_canonical {
                canonical_path_key(&name.relative_path.to_string_lossy())
            } else {
                name.relative_path.to_string_lossy().into_owned()
            };
            collisions.entry(key).or_default().push(index);
        }
        for indexes in collisions.into_values().filter(|v| v.len() > 1) {
            for index in indexes {
                let suffix = &requests[index].clip.id;
                rendered[index] = with_suffix(
                    rendered[index].clone(),
                    suffix,
                    config.character_set,
                    config.max_component_len,
                );
            }
        }
    }

    rendered
}

/// The album path component for every request, with a clip whose root title
/// collides across distinct roots disambiguated by `[{root_id8}]`.
///
/// Distinct roots must never share an album folder. `colliding_albums` is the
/// authoritative set of such shared root titles, computed once from the whole
/// lineage store, so the decision is stable across runs and independent of the
/// batch. A clip whose resolved album is in that set gets its root's short id
/// appended; every other clip keeps the bare album and groups with its
/// same-root siblings.
fn disambiguated_albums(
    requests: &[NamingRequest<'_>],
    config: &NamingConfig,
    colliding_albums: &BTreeSet<String>,
) -> Vec<String> {
    requests
        .iter()
        .map(|request| album_for(*request, config, colliding_albums))
        .collect()
}

/// The (possibly disambiguated) album component for one request.
fn album_for(
    request: NamingRequest<'_>,
    config: &NamingConfig,
    colliding_albums: &BTreeSet<String>,
) -> String {
    let raw_album = request.lineage.album(&title_name(request.clip));
    let album = sanitise_component(&raw_album, config.character_set, config.max_component_len);
    if colliding_albums.contains(raw_album.trim()) {
        let suffix = truncate_chars(&request.lineage.root_id, 8);
        append_suffix(
            &album,
            &suffix,
            config.character_set,
            config.max_component_len,
        )
    } else {
        album
    }
}

/// The sanitised album component: the resolved lineage album (root title, else
/// the clip's own title).
fn album_component(request: NamingRequest<'_>, config: &NamingConfig) -> String {
    let album = request.lineage.album(&title_name(request.clip));
    sanitise_component(&album, config.character_set, config.max_component_len)
}

/// Render one clip's path with an already-resolved album component.
fn render_with_album(
    request: NamingRequest<'_>,
    config: &NamingConfig,
    album: &str,
) -> RenderedName {
    let clip = request.clip;
    let creator = sanitise_component(
        &creator_name(clip),
        config.character_set,
        config.max_component_len,
    );
    let handle = sanitise_component(&clip.handle, config.character_set, config.max_component_len);
    let title = sanitise_component(
        &title_name(clip),
        config.character_set,
        config.max_component_len,
    );
    let id = sanitise_component(&clip.id, CharacterSet::Ascii, config.max_component_len);
    let id8 = sanitise_component(
        &truncate_chars(&clip.id, 8),
        CharacterSet::Ascii,
        config.max_component_len,
    );
    let root_id8 = sanitise_component(
        &truncate_chars(&request.lineage.root_id, 8),
        CharacterSet::Ascii,
        config.max_component_len,
    );
    let track = request.lineage.track;
    let track_raw = if track > 0 {
        track.to_string()
    } else {
        String::new()
    };
    let track_pad = if track > 0 {
        format!("{track:02}")
    } else {
        String::new()
    };
    let substitutions = SegmentSubstitutions {
        creator: &creator,
        handle: &handle,
        album,
        title: &title,
        root_id8: &root_id8,
        id8: &id8,
        id: &id,
        track: &track_raw,
        track2: &track_pad,
    };
    let mut components = config
        .template
        .split('/')
        .filter_map(|segment| {
            let rendered = substitute_segment(segment, substitutions);
            let sanitised = sanitise_segment(
                &rendered,
                config.character_set,
                config.max_component_len,
                [id8.as_str(), root_id8.as_str()],
            );
            (!sanitised.is_empty()).then_some(sanitised)
        })
        .collect::<Vec<_>>();

    if components.is_empty() {
        components.push(title.clone());
    }

    let mut base_name = components
        .pop()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| title.clone());
    // Guarantee a non-empty file name even when every token sanitises away.
    if base_name.is_empty() {
        base_name = append_suffix(
            &base_name,
            &clip.id,
            config.character_set,
            config.max_component_len,
        );
    }

    let mut relative_path = PathBuf::new();
    for component in components {
        relative_path.push(component);
    }

    relative_path.push(&base_name);
    RenderedName {
        relative_path,
        base_name,
    }
}

#[derive(Clone, Copy)]
struct SegmentSubstitutions<'a> {
    creator: &'a str,
    handle: &'a str,
    album: &'a str,
    title: &'a str,
    root_id8: &'a str,
    id8: &'a str,
    id: &'a str,
    track: &'a str,
    track2: &'a str,
}

fn substitute_segment(segment: &str, substitutions: SegmentSubstitutions<'_>) -> String {
    let mut rendered = String::with_capacity(segment.len());
    let mut remainder = segment;
    while let Some(start) = remainder.find('{') {
        rendered.push_str(&remainder[..start]);
        remainder = &remainder[start..];
        if let Some((token_len, value)) = placeholder_match(remainder, substitutions) {
            rendered.push_str(value);
            remainder = &remainder[token_len..];
            // An empty placeholder swallows the separator run that follows it, so
            // an optional token (e.g. an unnumbered `{track2}`) leaves no orphan
            // separator like a leading " - ".
            if value.is_empty() {
                remainder = remainder.trim_start_matches([' ', '-', '_', '.']);
            }
        } else {
            rendered.push('{');
            remainder = &remainder[1..];
        }
    }
    rendered.push_str(remainder);
    rendered
}

fn placeholder_match<'a>(
    segment: &str,
    substitutions: SegmentSubstitutions<'a>,
) -> Option<(usize, &'a str)> {
    if segment.starts_with("{creator}") {
        Some(("{creator}".len(), substitutions.creator))
    } else if segment.starts_with("{handle}") {
        Some(("{handle}".len(), substitutions.handle))
    } else if segment.starts_with("{album}") {
        Some(("{album}".len(), substitutions.album))
    } else if segment.starts_with("{title}") {
        Some(("{title}".len(), substitutions.title))
    } else if segment.starts_with("{root_id8}") {
        Some(("{root_id8}".len(), substitutions.root_id8))
    } else if segment.starts_with("{id8}") {
        Some(("{id8}".len(), substitutions.id8))
    } else if segment.starts_with("{id}") {
        Some(("{id}".len(), substitutions.id))
    } else if segment.starts_with("{track2}") {
        Some(("{track2}".len(), substitutions.track2))
    } else if segment.starts_with("{track}") {
        Some(("{track}".len(), substitutions.track))
    } else {
        None
    }
}

fn with_suffix(
    mut rendered: RenderedName,
    suffix: &str,
    character_set: CharacterSet,
    max_component_len: usize,
) -> RenderedName {
    rendered.base_name = append_suffix(
        &rendered.base_name,
        suffix,
        character_set,
        max_component_len,
    );
    rendered.relative_path.set_file_name(&rendered.base_name);
    rendered
}

fn creator_name(clip: &Clip) -> String {
    non_blank(&clip.display_name)
        .or_else(|| non_blank(&clip.handle))
        .unwrap_or("Unknown Creator")
        .to_string()
}

fn title_name(clip: &Clip) -> String {
    let title = clip.title.trim();
    if title.is_empty() || title.eq_ignore_ascii_case("untitled") {
        "Untitled".to_string()
    } else {
        title.to_string()
    }
}

fn append_suffix(
    base: &str,
    suffix: &str,
    character_set: CharacterSet,
    max_component_len: usize,
) -> String {
    let suffix_pattern = format!(" [{suffix}]");
    if base.ends_with(&suffix_pattern) {
        return sanitise_component(base, character_set, max_component_len);
    }

    let max_len =
        max_component_len.max(suffix_pattern.chars().count() + MIN_BASE_CHARS_WITH_SUFFIX);
    let allowed = max_len.saturating_sub(suffix_pattern.chars().count());
    // Sanitise the base before measuring it. The character set can expand a
    // character (ascii turns `ß` into `ss`), so budgeting the cut on the raw
    // length could let the sanitised prefix grow back over the room reserved for
    // the suffix and slice through it again (#120).
    let base = sanitise_component(base, character_set, max_len);
    let truncated = truncate_chars(base.trim_end(), allowed);
    let combined = format!("{truncated}{suffix_pattern}");
    sanitise_component(&combined, character_set, max_len)
}

/// Sanitise a rendered template segment, preserving a trailing ` [id]`
/// disambiguator (the `[{id8}]` or `[{root_id8}]` the template embeds) when the
/// segment would otherwise be truncated through it. Only the title portion is
/// shortened, so two long-titled siblings keep their distinguishing id and the
/// closing bracket is never left unbalanced (#120). A segment that does not end
/// in a disambiguator is sanitised exactly as before.
fn sanitise_segment(
    rendered: &str,
    character_set: CharacterSet,
    max_component_len: usize,
    disambiguators: [&str; 2],
) -> String {
    for suffix in disambiguators {
        if suffix.is_empty() {
            continue;
        }
        let pattern = format!(" [{suffix}]");
        if let Some(prefix) = rendered.strip_suffix(&pattern) {
            return append_suffix(prefix, suffix, character_set, max_component_len);
        }
    }
    sanitise_component(rendered, character_set, max_component_len)
}

/// Sanitise a free-form playlist name into a single safe path component.
///
/// Applies the same Unicode filtering and length cap as clip path components
/// (default [`CharacterSet::Unicode`], [`DEFAULT_MAX_COMPONENT_LEN`]), so a
/// playlist file name obeys the same filesystem rules as the rest of the
/// library. An empty or fully-stripped name falls back to `playlist` so the
/// caller always has a non-empty stem to append `.m3u8` to.
pub fn sanitise_name(name: &str) -> String {
    let cleaned = sanitise_component(name, CharacterSet::Unicode, DEFAULT_MAX_COMPONENT_LEN);
    if cleaned.is_empty() {
        "playlist".to_string()
    } else {
        cleaned
    }
}

/// The `.stems` sub-folder that sits beside a song's audio file.
///
/// `base` is the song's extensionless relative path (the same value the audio
/// and its sidecars are built from), so the folder is `{base}.stems`. It cannot
/// collide with the audio file (`{base}.<ext>`) or any `{base}.<sidecar>`
/// because the `.stems` suffix is distinct, mirroring the sidecar convention.
pub fn stems_folder(base: &str) -> String {
    format!("{base}.stems")
}

/// The relative path of one stem file inside a song's [`stems_folder`].
///
/// Base+label+disambiguation rather than label-only, because Auto Split can
/// mislabel stems and Advanced Split yields ~100 instruments, so blank or
/// duplicate labels are expected. The file is
/// `{song file name} - {label} [{stem id8}].{ext}`; ` - {label}` is dropped when
/// the label sanitises to empty, and the `[{stem id8}]` disambiguator (first 8
/// of the stable stem id) keeps blank or duplicate labels collision-free. Every
/// component runs through the same [`sanitise_component`] filter, honouring
/// `character_set`.
pub fn stem_file_path(
    base: &str,
    label: &str,
    stem_id: &str,
    ext: &str,
    character_set: CharacterSet,
) -> String {
    let folder = stems_folder(base);
    // The song's own file-name stem (the last path component of `base`), reused
    // so a stem stays identifiable even when viewed outside its `.stems` folder.
    let song_stem = base.rsplit('/').next().unwrap_or(base);
    let label = sanitise_component(label, character_set, DEFAULT_MAX_COMPONENT_LEN);
    let id8 = sanitise_component(
        &truncate_chars(stem_id, 8),
        CharacterSet::Ascii,
        DEFAULT_MAX_COMPONENT_LEN,
    );

    let mut name = song_stem.to_string();
    if !label.is_empty() {
        name.push_str(" - ");
        name.push_str(&label);
    }
    if !id8.is_empty() {
        name.push_str(" [");
        name.push_str(&id8);
        name.push(']');
    }
    // A degenerate base (empty song stem, blank label, empty id) must still
    // yield a usable name rather than a hidden dotfile.
    if name.trim().is_empty() {
        name = "stem".to_string();
    }
    format!("{folder}/{name}.{}", sanitise_ext(ext))
}

/// Reduce a candidate extension to a safe lowercase alphanumeric token,
/// defaulting to `mp3` when it is empty or fully stripped. The caller passes the
/// resolved stem format's extension (`wav` or `mp3`); stems are stored RAW.
fn sanitise_ext(ext: &str) -> String {
    let cleaned: String = ext
        .trim_start_matches('.')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .take(8)
        .collect();
    if cleaned.is_empty() {
        "mp3".to_string()
    } else {
        cleaned
    }
}

fn sanitise_component(
    value: &str,
    character_set: CharacterSet,
    max_component_len: usize,
) -> String {
    // Single pass: map each char to its charset-safe form, collapsing runs of
    // whitespace to one space and dropping leading/trailing whitespace.
    let mut collapsed = String::with_capacity(value.len());
    let mut pending_space = false;
    let push = |out: char, buf: &mut String, pending: &mut bool| {
        if out.is_whitespace() {
            *pending = !buf.is_empty();
        } else {
            if *pending {
                buf.push(' ');
            }
            *pending = false;
            buf.push(out);
        }
    };
    match character_set {
        CharacterSet::Unicode => {
            for ch in value.chars() {
                push(unicode_char(ch), &mut collapsed, &mut pending_space);
            }
        }
        CharacterSet::Ascii => {
            for ch in value.chars() {
                for out in ascii_chars(ch) {
                    push(out, &mut collapsed, &mut pending_space);
                }
            }
        }
    }

    let trimmed = collapsed.trim_matches([' ', '.']);
    if trimmed.is_empty() {
        return String::new();
    }

    // Keep at most `max` characters, then trim any space or dot the cut exposed.
    // Slice at a char boundary rather than truncating a copy.
    let max = max_component_len.max(1);
    let end = trimmed
        .char_indices()
        .nth(max)
        .map_or(trimmed.len(), |(index, _)| index);
    let result = trimmed[..end].trim_matches([' ', '.']);
    if result.is_empty() {
        return String::new();
    }
    if result == "." || result == ".." {
        return "item".to_string();
    }
    let mut result = result.to_string();
    if is_reserved_name(&result) {
        // Guard the stem, not the whole component: `NUL.mp3` must become
        // `NUL_.mp3`, since `NUL.mp3_` keeps `NUL` as its dot-stem and stays a
        // Windows device name.
        let stem_end = result.find('.').unwrap_or(result.len());
        result.insert(stem_end, '_');
    }
    result
}

fn unicode_char(ch: char) -> char {
    if matches!(
        ch,
        '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0'
    ) || ch.is_control()
    {
        ' '
    } else {
        ch
    }
}

fn ascii_chars(ch: char) -> Vec<char> {
    if ch.is_ascii() {
        return vec![unicode_char(ch)];
    }

    match ch {
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => vec!['A'],
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => vec!['a'],
        'Ç' => vec!['C'],
        'ç' => vec!['c'],
        'È' | 'É' | 'Ê' | 'Ë' => vec!['E'],
        'è' | 'é' | 'ê' | 'ë' => vec!['e'],
        'Ì' | 'Í' | 'Î' | 'Ï' => vec!['I'],
        'ì' | 'í' | 'î' | 'ï' => vec!['i'],
        'Ñ' => vec!['N'],
        'ñ' => vec!['n'],
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => vec!['O'],
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => vec!['o'],
        'Ù' | 'Ú' | 'Û' | 'Ü' => vec!['U'],
        'ù' | 'ú' | 'û' | 'ü' => vec!['u'],
        'Ý' | 'Ÿ' => vec!['Y'],
        'ý' | 'ÿ' => vec!['y'],
        'Æ' => vec!['A', 'E'],
        'æ' => vec!['a', 'e'],
        'Œ' => vec!['O', 'E'],
        'œ' => vec!['o', 'e'],
        'ß' => vec!['s', 's'],
        _ => vec![' '],
    }
}

fn truncate_chars(value: &str, max_len: usize) -> String {
    value.chars().take(max_len).collect()
}

fn non_blank(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Whether `value`'s stem is a Windows reserved device name (`CON`, `PRN`,
/// `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`), matched case-insensitively.
///
/// Exposed to the crate so the naming-safety fuzz can assert no rendered path
/// component is ever a reserved name; the sanitiser guarantees it at
/// [`sanitise_component`].
pub(crate) fn is_reserved_name(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    // Every reserved device name is 3 (CON/PRN/AUX/NUL) or 4 (COMx/LPTx) ASCII
    // bytes, so anything else cannot match without allocating an uppercased copy.
    if !matches!(stem.len(), 3 | 4) {
        return false;
    }
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    RESERVED.iter().any(|name| name.eq_ignore_ascii_case(stem))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod proptests;
