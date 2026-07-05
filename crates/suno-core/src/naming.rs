//! Pure naming and relative path rendering for [`Clip`] values.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization as _;

use crate::Clip;
use crate::error::{Error, Result};
use crate::lineage::LineageContext;

/// The default relative path template.
///
/// Supported placeholders are `{creator}`, `{handle}`, `{album}`, `{title}`,
/// `{id}`, `{id8}` (first 8 characters of the clip id), and `{root_id8}`
/// (first 8 of the resolved lineage root id). Empty path segments are dropped
/// after rendering.
///
/// The default embeds `[{id8}]` in the file name so same-title clips never
/// collide, and folders under `{album}`, which resolves to the lineage root's
/// title (else the clip's own title).
pub const DEFAULT_TEMPLATE: &str = "{creator}/{album}/{creator}-{title} [{id8}]";
const DEFAULT_MAX_COMPONENT_LEN: usize = 80;

const MIN_BASE_CHARS_WITH_SUFFIX: usize = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CharacterSet {
    #[default]
    Unicode,
    Ascii,
}

impl FromStr for CharacterSet {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
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

pub fn render_clip_name(request: NamingRequest<'_>, config: &NamingConfig) -> RenderedName {
    let album = album_component(request, config);
    render_with_album(request, config, &album)
}

pub fn render_clip_names(
    requests: &[NamingRequest<'_>],
    config: &NamingConfig,
    colliding_albums: &BTreeSet<String>,
) -> Vec<RenderedName> {
    let albums = disambiguated_albums(requests, config, colliding_albums);
    let mut rendered = requests
        .iter()
        .zip(&albums)
        .map(|(request, album)| render_with_album(*request, config, album))
        .collect::<Vec<_>>();

    // Two passes to keep distinct clips from landing on one path.  The first
    // pass keys on the exact rendered string; the second on the filesystem-
    // canonical form (NFC + lowercase) so that paths differing only by case or
    // Unicode normalisation (NFD vs NFC) are caught too — they would collide on
    // case-insensitive or NFC-normalising filesystems (Windows, macOS default).
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

/// Filesystem-canonical key: NFC-normalise then lowercase, so paths that differ
/// only by case or by NFC/NFD encoding hash to the same bucket.
fn canonical_path_key(path: &str) -> String {
    path.nfc().flat_map(char::to_lowercase).collect()
}

/// The album path component for every request, with a clip whose root title
/// collides across distinct roots disambiguated by `[{root_id8}]`.
///
/// Distinct roots must never share an album folder (two different upload roots
/// titled "Break Through" exist). `colliding_albums` is the authoritative set
/// of such shared root titles, computed once from the whole lineage store, so
/// the decision is stable across runs and independent of which clips appear in
/// this batch. A clip whose resolved album is in that set always gets its
/// root's short id appended; every other clip keeps the bare album and groups
/// with its same-root siblings.
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
    let substitutions = SegmentSubstitutions {
        creator: &creator,
        handle: &handle,
        album,
        title: &title,
        root_id8: &root_id8,
        id8: &id8,
        id: &id,
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
/// Named base+label+disambiguation rather than label-only, because Auto Split
/// can mislabel stems and Advanced Split yields ~100 instruments, so blank or
/// duplicate labels are expected. The file is
/// `{song file name} - {label} [{stem id8}].{ext}`; the ` - {label}` piece is
/// dropped when the label sanitises to empty, and the `[{stem id8}]`
/// disambiguator (the first 8 characters of the stable stem id) keeps blank or
/// duplicate labels collision-free. Every component is run through the same
/// [`sanitise_component`] filter as the rest of the library, honouring
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
    // Single pass: map each char to its charset-safe form while collapsing runs
    // of whitespace to one space and dropping leading/trailing whitespace. This
    // fuses the old filter / split_whitespace / collect / join steps, which
    // allocated several intermediate strings and a vector, into one buffer.
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
    // Slicing at the char boundary avoids the extra String the old
    // truncate-then-trim-then-to_string sequence built.
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
    if !result.ends_with('_') && is_reserved_name(&result) {
        result.push('_');
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

fn is_reserved_name(value: &str) -> bool {
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
mod tests {
    use super::*;
    use crate::lineage::{EdgeType, ResolveStatus};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    fn test_clip(id: &str, title: &str) -> Clip {
        Clip {
            id: id.to_string(),
            title: title.to_string(),
            display_name: "München".to_string(),
            handle: "munchen".to_string(),
            ..Clip::default()
        }
    }

    fn render_own(clip: &Clip, config: &NamingConfig) -> RenderedName {
        let lineage = LineageContext::own_root(clip);
        render_clip_name(
            NamingRequest {
                clip,
                lineage: &lineage,
            },
            config,
        )
    }

    fn render_all_own(
        clips: &[Clip],
        config: &NamingConfig,
        colliding: &BTreeSet<String>,
    ) -> Vec<RenderedName> {
        let lineages: Vec<LineageContext> = clips.iter().map(LineageContext::own_root).collect();
        let requests: Vec<NamingRequest> = clips
            .iter()
            .zip(&lineages)
            .map(|(clip, lineage)| NamingRequest { clip, lineage })
            .collect();
        render_clip_names(&requests, config, colliding)
    }

    #[test]
    fn unicode_names_are_preserved_and_ascii_falls_back() {
        let clip = test_clip("abc12345", "Beyoncé/東京");

        let unicode = render_own(&clip, &NamingConfig::default());
        assert_eq!(
            unicode.relative_path,
            Path::new("München/Beyoncé 東京/München-Beyoncé 東京 [abc12345]")
        );

        let ascii = render_own(
            &clip,
            &NamingConfig {
                character_set: CharacterSet::Ascii,
                ..NamingConfig::default()
            },
        );
        assert_eq!(
            ascii.relative_path,
            Path::new("Munchen/Beyonce/Munchen-Beyonce [abc12345]")
        );
    }

    #[test]
    fn reserved_and_hostile_names_are_sanitised() {
        let clip = Clip {
            id: "deadbeef".to_string(),
            title: "CON<>:\"/\\|?*.".to_string(),
            display_name: "AUX".to_string(),
            ..Clip::default()
        };

        let rendered = render_own(&clip, &NamingConfig::default());
        assert!(
            rendered.relative_path.starts_with("AUX_/CON_"),
            "path was {}",
            rendered.relative_path.display()
        );
        assert!(rendered.base_name.contains("[deadbeef]"));
    }

    #[test]
    fn default_template_always_embeds_id8() {
        let clip = test_clip("abcdef1234567890", "Any Title");
        let rendered = render_own(&clip, &NamingConfig::default());
        assert!(
            rendered.base_name.contains("[abcdef12]"),
            "base_name was {}",
            rendered.base_name
        );
    }

    #[test]
    fn custom_template_replaces_all_known_placeholders_once() {
        let clip = Clip {
            id: "abcdef12-full".to_string(),
            title: "Song".to_string(),
            display_name: "Creator".to_string(),
            handle: "handle".to_string(),
            ..Clip::default()
        };
        let lineage = LineageContext {
            root_id: "rootxyz9-extra".to_string(),
            root_title: "Album".to_string(),
            root_date: String::new(),
            parent_id: "rootxyz9-extra".to_string(),
            edge_type: Some(EdgeType::Cover),
            status: ResolveStatus::Resolved,
        };
        let config = NamingConfig {
            template: "{creator}-{handle}-{album}-{title}-{root_id8}-{id8}-{id}-{unknown}"
                .to_string(),
            ..NamingConfig::default()
        };

        let rendered = render_clip_name(
            NamingRequest {
                clip: &clip,
                lineage: &lineage,
            },
            &config,
        );

        assert_eq!(
            rendered.relative_path.to_string_lossy(),
            "Creator-handle-Album-Song-rootxyz9-abcdef12-abcdef12-full-{unknown}"
        );
    }

    #[test]
    fn blank_titles_use_a_stable_suffix() {
        let clip = test_clip("12345678-clip", "   ");

        let rendered = render_own(&clip, &NamingConfig::default());
        assert_eq!(rendered.base_name, "München-Untitled [12345678]");
        assert_eq!(
            rendered.relative_path,
            Path::new("München/Untitled/München-Untitled [12345678]")
        );
    }

    #[test]
    fn very_long_titles_are_trimmed() {
        let clip = test_clip("abcdef12", &"a".repeat(120));
        let rendered = render_own(
            &clip,
            &NamingConfig {
                max_component_len: 24,
                ..NamingConfig::default()
            },
        );

        for component in rendered.relative_path.components() {
            let text = component.as_os_str().to_string_lossy();
            assert!(
                text.chars().count() <= 24,
                "component {text:?} exceeds 24 chars"
            );
        }
        // The trailing [id8] must survive the truncation intact (#120).
        assert!(
            rendered.base_name.ends_with(" [abcdef12]"),
            "id8 disambiguator was sliced; base_name was {:?}",
            rendered.base_name
        );
    }

    #[test]
    fn long_names_keep_the_full_id8_disambiguator() {
        // A creator+title long enough to overflow the cap keeps the whole
        // trailing [id8]: the title is shortened, not the id, so the name stays
        // complete and the bracket stays balanced (#120).
        let clip = test_clip("1234abcd-tail", &"a".repeat(120));
        let config = NamingConfig {
            max_component_len: 40,
            ..NamingConfig::default()
        };
        let rendered = render_own(&clip, &config);

        assert!(
            rendered.base_name.ends_with(" [1234abcd]"),
            "base_name must end with the full disambiguator, was {:?}",
            rendered.base_name
        );
        assert_eq!(rendered.base_name.chars().count(), 40);
    }

    #[test]
    fn long_titled_siblings_stay_distinct_with_balanced_brackets() {
        // Two same-(long-)titled clips sharing a root must remain distinct: only
        // the title is shortened, so their [id8] suffixes differ and neither name
        // ends up with an unbalanced bracket (#120).
        let lineage = LineageContext {
            root_id: "root-42".to_string(),
            root_title: "Origin".to_string(),
            root_date: String::new(),
            parent_id: "root-42".to_string(),
            edge_type: Some(EdgeType::Cover),
            status: ResolveStatus::Resolved,
        };
        let title = "z".repeat(200);
        let first = test_clip("aaaa1111-x", &title);
        let second = test_clip("bbbb2222-y", &title);
        let requests = [
            NamingRequest {
                clip: &first,
                lineage: &lineage,
            },
            NamingRequest {
                clip: &second,
                lineage: &lineage,
            },
        ];

        let names = render_clip_names(&requests, &NamingConfig::default(), &BTreeSet::new());

        assert!(names[0].base_name.ends_with(" [aaaa1111]"));
        assert!(names[1].base_name.ends_with(" [bbbb2222]"));
        assert_ne!(names[0].relative_path, names[1].relative_path);
        for name in &names {
            assert!(name.base_name.chars().count() <= 80);
            assert_eq!(name.base_name.matches('[').count(), 1, "unbalanced '['");
            assert_eq!(name.base_name.matches(']').count(), 1, "unbalanced ']'");
        }
    }

    #[test]
    fn long_colliding_album_keeps_its_root_id8() {
        // The album [root_id8] disambiguator is preserved when a long album title
        // must be truncated, mirroring the file-name fix (#120).
        let long = "Break Through ".repeat(20);
        let title = long.trim().to_string();
        let clip = Clip {
            id: "aaaa1111-x".to_string(),
            title: title.clone(),
            display_name: "München".to_string(),
            ..Clip::default()
        };
        let colliding: BTreeSet<String> = [title].into_iter().collect();
        let names = render_all_own(&[clip], &NamingConfig::default(), &colliding);

        let album = names[0]
            .relative_path
            .components()
            .nth(1)
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(album.ends_with(" [aaaa1111]"), "album was {album:?}");
        assert!(album.chars().count() <= 80);
    }

    #[test]
    fn ascii_expanding_chars_do_not_slice_the_disambiguator() {
        // A literal expanding character (`ß` -> `ss` under ascii) in a custom
        // template, right before the trailing ` [{id8}]`, must not grow back over
        // the suffix and slice it: the base is sized after expansion (#120).
        let clip = test_clip("1234abcd", "Title");
        let config = NamingConfig {
            template: format!("{}{{title}} [{{id8}}]", "ß".repeat(80)),
            character_set: CharacterSet::Ascii,
            max_component_len: 40,
        };
        let rendered = render_own(&clip, &config);

        assert!(
            rendered.base_name.ends_with(" [1234abcd]"),
            "expansion sliced the id8; base_name was {:?}",
            rendered.base_name
        );
        assert!(rendered.base_name.chars().count() <= 40);
    }

    #[test]
    fn same_title_siblings_stay_distinct_via_id8() {
        // Two clips sharing a root (same album folder) and the same title must
        // still land on distinct files; the default template's {id8} does that.
        let lineage = LineageContext {
            root_id: "root-9".to_string(),
            root_title: "Origin".to_string(),
            root_date: String::new(),
            parent_id: "root-9".to_string(),
            edge_type: Some(EdgeType::Cover),
            status: ResolveStatus::Resolved,
        };
        let first = test_clip("11111111-alpha", "Shared");
        let second = test_clip("22222222-beta", "Shared");
        let requests = [
            NamingRequest {
                clip: &first,
                lineage: &lineage,
            },
            NamingRequest {
                clip: &second,
                lineage: &lineage,
            },
        ];

        let names = render_clip_names(&requests, &NamingConfig::default(), &BTreeSet::new());

        assert_eq!(
            names[0].relative_path,
            Path::new("München/Origin/München-Shared [11111111]")
        );
        assert_eq!(
            names[1].relative_path,
            Path::new("München/Origin/München-Shared [22222222]")
        );
    }

    #[test]
    fn id8_prefix_collision_falls_back_to_full_id() {
        // Custom template without {id8} so identical titles collide and the
        // filename fallback (full id) has to keep them distinct.
        let config = NamingConfig {
            template: "{creator}/{title}".to_string(),
            ..NamingConfig::default()
        };
        let first = test_clip("abcd1234-first", "Untitled");
        let second = test_clip("abcd1234-second", "Untitled");

        let names = render_all_own(&[first.clone(), second.clone()], &config, &BTreeSet::new());
        let swapped = render_all_own(&[second.clone(), first.clone()], &config, &BTreeSet::new());

        assert_ne!(
            names[0].relative_path.to_string_lossy(),
            names[1].relative_path.to_string_lossy()
        );

        let ordered = |rendered: &[RenderedName], clips: &[Clip]| {
            clips
                .iter()
                .zip(rendered)
                .map(|(clip, name)| {
                    (
                        clip.id.clone(),
                        name.relative_path.to_string_lossy().into_owned(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            ordered(&names, &[first.clone(), second.clone()]),
            ordered(&swapped, &[second, first])
        );
    }

    #[test]
    fn album_is_root_title_for_a_remix() {
        let clip = Clip {
            id: "child".to_string(),
            title: "Remix".to_string(),
            display_name: "München".to_string(),
            ..Clip::default()
        };
        let lineage = LineageContext {
            root_id: "root-1".to_string(),
            root_title: "Original".to_string(),
            root_date: String::new(),
            parent_id: "root-1".to_string(),
            edge_type: Some(EdgeType::Cover),
            status: ResolveStatus::Resolved,
        };

        let rendered = render_clip_name(
            NamingRequest {
                clip: &clip,
                lineage: &lineage,
            },
            &NamingConfig::default(),
        );
        assert_eq!(
            rendered.relative_path,
            Path::new("München/Original/München-Remix [child]")
        );
    }

    #[test]
    fn album_is_own_title_for_a_root() {
        let clip = Clip {
            id: "root-1".to_string(),
            title: "Original".to_string(),
            display_name: "München".to_string(),
            ..Clip::default()
        };

        let rendered = render_own(&clip, &NamingConfig::default());
        assert_eq!(
            rendered.relative_path,
            Path::new("München/Original/München-Original [root-1]")
        );
    }

    #[test]
    fn shared_album_title_from_distinct_roots_is_disambiguated() {
        let first = Clip {
            id: "aaaa1111-x".to_string(),
            title: "Break Through".to_string(),
            display_name: "München".to_string(),
            ..Clip::default()
        };
        let second = Clip {
            id: "bbbb2222-y".to_string(),
            title: "Break Through".to_string(),
            display_name: "München".to_string(),
            ..Clip::default()
        };

        // The colliding set is authoritative (store-driven), so disambiguation
        // does not depend on both roots appearing in the same batch.
        let colliding: BTreeSet<String> = ["Break Through".to_string()].into_iter().collect();
        let names = render_all_own(
            &[first.clone(), second.clone()],
            &NamingConfig::default(),
            &colliding,
        );
        let swapped = render_all_own(
            &[second.clone(), first.clone()],
            &NamingConfig::default(),
            &colliding,
        );

        let album_of = |rendered: &RenderedName| {
            rendered
                .relative_path
                .components()
                .nth(1)
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_default()
        };

        assert_eq!(album_of(&names[0]), "Break Through [aaaa1111]");
        assert_eq!(album_of(&names[1]), "Break Through [bbbb2222]");
        // Deterministic regardless of input order.
        assert_eq!(album_of(&swapped[0]), "Break Through [bbbb2222]");
        assert_eq!(album_of(&swapped[1]), "Break Through [aaaa1111]");

        // The MEDIUM fix: a narrowed run showing only one of the two roots
        // still gets the suffixed folder, so folders never oscillate.
        let alone = render_all_own(
            std::slice::from_ref(&first),
            &NamingConfig::default(),
            &colliding,
        );
        assert_eq!(album_of(&alone[0]), "Break Through [aaaa1111]");
    }

    #[test]
    fn unique_root_title_stays_a_bare_album() {
        // A title absent from the colliding set keeps its bare folder even when
        // the batch happens to hold a same-titled sibling of the same root.
        let clip = Clip {
            id: "solo-1".to_string(),
            title: "Solo".to_string(),
            display_name: "München".to_string(),
            ..Clip::default()
        };
        let names = render_all_own(&[clip], &NamingConfig::default(), &BTreeSet::new());
        assert_eq!(
            names[0].relative_path,
            Path::new("München/Solo/München-Solo [solo-1]")
        );
    }

    #[test]
    fn sanitise_name_strips_separators_and_falls_back_when_empty() {
        assert_eq!(sanitise_name("Road/Trip: 2024"), "Road Trip 2024");
        assert_eq!(sanitise_name(""), "playlist");
        // A name made only of illegal characters strips to nothing, so the
        // caller still gets a usable, non-empty stem.
        assert_eq!(sanitise_name("///"), "playlist");
    }

    #[test]
    fn stems_folder_is_a_sibling_suffix_of_the_song_base() {
        assert_eq!(
            stems_folder("Creator/Album/Creator-Song [abcd1234]"),
            "Creator/Album/Creator-Song [abcd1234].stems"
        );
    }

    #[test]
    fn stem_file_path_combines_song_stem_label_and_disambiguator() {
        let path = stem_file_path(
            "Creator/Album/Creator-Song [abcd1234]",
            "Vocals",
            "stem-vocals-9f8e7d6c",
            "mp3",
            CharacterSet::Unicode,
        );
        assert_eq!(
            path,
            "Creator/Album/Creator-Song [abcd1234].stems/Creator-Song [abcd1234] - Vocals [stem-voc].mp3"
        );
    }

    #[test]
    fn stem_file_path_disambiguates_blank_and_duplicate_labels_by_id() {
        // Two stems with the SAME (blank) label must not collide: the stem-id
        // disambiguator keeps them distinct even with no usable label.
        let a = stem_file_path("song", "", "id-aaaaaaaa", "wav", CharacterSet::Unicode);
        let b = stem_file_path("song", "", "id-bbbbbbbb", "wav", CharacterSet::Unicode);
        assert_eq!(a, "song.stems/song [id-aaaaa].wav");
        assert_eq!(b, "song.stems/song [id-bbbbb].wav");
        assert_ne!(a, b);
    }

    #[test]
    fn stem_file_path_sanitises_label_and_extension_and_honours_ascii() {
        // Illegal path characters in the label are stripped, the extension is
        // reduced to a safe lowercase token, and ASCII folding applies.
        let path = stem_file_path(
            "song",
            "Lead/Vocal: Æ",
            "STEMID12",
            ".FLAC",
            CharacterSet::Ascii,
        );
        assert_eq!(path, "song.stems/song - Lead Vocal AE [STEMID12].flac");
        // A junk extension falls back to mp3 (defensive; callers pass wav/mp3).
        let fallback = stem_file_path("s", "Bass", "x", "??", CharacterSet::Unicode);
        assert_eq!(fallback, "s.stems/s - Bass [x].mp3");
    }

    #[test]
    fn case_only_path_difference_is_a_canonical_collision() {
        // A custom template without {id8}: clips whose titles differ only in
        // case produce different exact paths but the same canonical path and
        // must be disambiguated to avoid clobbering on case-insensitive FSes.
        let config = NamingConfig {
            template: "{creator}/{title}".to_string(),
            ..NamingConfig::default()
        };
        let first = test_clip("aaaa1111-x", "sunrise");
        let second = test_clip("bbbb2222-y", "SUNRISE");

        let names = render_all_own(&[first, second], &config, &BTreeSet::new());

        assert_ne!(
            names[0].relative_path.to_string_lossy(),
            names[1].relative_path.to_string_lossy(),
            "canonical collision was not disambiguated"
        );
    }

    #[test]
    fn nfc_nfd_path_difference_is_a_canonical_collision() {
        // The same character encoded as NFC vs NFD produces different byte
        // strings but the same file on NFC-normalising filesystems (macOS APFS).
        let config = NamingConfig {
            template: "{creator}/{title}".to_string(),
            ..NamingConfig::default()
        };
        // "é" as NFC (U+00E9) vs NFD (e + U+0301).
        let nfc_title = "\u{00e9}toile";
        let nfd_title = "e\u{0301}toile";
        let first = test_clip("aaaa1111-x", nfc_title);
        let second = test_clip("bbbb2222-y", nfd_title);

        let names = render_all_own(&[first, second], &config, &BTreeSet::new());

        assert_ne!(
            names[0].relative_path.to_string_lossy(),
            names[1].relative_path.to_string_lossy(),
            "NFC/NFD canonical collision was not disambiguated"
        );
    }

    #[test]
    fn genuinely_distinct_paths_are_never_wrongly_disambiguated() {
        // Clips with distinct titles (not even canonically equivalent) must not
        // receive unnecessary suffixes — the canonical check must not produce
        // false positives.
        let config = NamingConfig {
            template: "{creator}/{title}".to_string(),
            ..NamingConfig::default()
        };
        let first = test_clip("aaaa1111-x", "Alpha");
        let second = test_clip("bbbb2222-y", "Beta");

        let names = render_all_own(&[first, second], &config, &BTreeSet::new());

        assert_eq!(
            names[0].relative_path,
            Path::new("München/Alpha"),
            "distinct path was wrongly suffixed"
        );
        assert_eq!(
            names[1].relative_path,
            Path::new("München/Beta"),
            "distinct path was wrongly suffixed"
        );
    }
}
