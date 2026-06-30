//! Pure naming and relative path rendering for [`Clip`] values.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::Clip;

/// The default relative path template.
///
/// Supported placeholders are `{creator}`, `{handle}`, `{album}`, `{title}`,
/// and `{id}`. Empty path segments are dropped after rendering.
pub const DEFAULT_TEMPLATE: &str = "{creator}/{album}/{title}";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CharacterSet {
    #[default]
    Unicode,
    Ascii,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AlbumMode {
    #[default]
    Lineage,
    Playlist,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamingConfig {
    pub template: String,
    pub character_set: CharacterSet,
    pub album_mode: AlbumMode,
    pub max_component_len: usize,
}

impl Default for NamingConfig {
    fn default() -> Self {
        Self {
            template: DEFAULT_TEMPLATE.to_string(),
            character_set: CharacterSet::Unicode,
            album_mode: AlbumMode::Lineage,
            max_component_len: 80,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NamingRequest<'a> {
    pub clip: &'a Clip,
    pub playlist_title: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedName {
    pub relative_path: PathBuf,
    pub base_name: String,
}

pub fn derive_album(
    clip: &Clip,
    playlist_title: Option<&str>,
    album_mode: AlbumMode,
) -> Option<String> {
    match album_mode {
        AlbumMode::Lineage => lineage_album(clip),
        AlbumMode::Playlist => playlist_title
            .and_then(non_blank)
            .map(str::to_string)
            .or_else(|| lineage_album(clip)),
    }
}

pub fn render_clip_name(request: NamingRequest<'_>, config: &NamingConfig) -> RenderedName {
    render_single(request, config)
}

pub fn render_clip_names(
    requests: &[NamingRequest<'_>],
    config: &NamingConfig,
) -> Vec<RenderedName> {
    let mut rendered = requests
        .iter()
        .copied()
        .map(|request| render_single(request, config))
        .collect::<Vec<_>>();
    let mut collisions = BTreeMap::<String, Vec<usize>>::new();

    for (index, name) in rendered.iter().enumerate() {
        collisions
            .entry(name.relative_path.to_string_lossy().into_owned())
            .or_default()
            .push(index);
    }

    for indexes in collisions.into_values().filter(|indexes| indexes.len() > 1) {
        for index in indexes {
            let suffix = short_id(requests[index].clip);
            rendered[index] =
                with_suffix(rendered[index].clone(), suffix, config.max_component_len);
        }
    }

    rendered
}

fn render_single(request: NamingRequest<'_>, config: &NamingConfig) -> RenderedName {
    let clip = request.clip;
    let creator = sanitise_component(
        &creator_name(clip),
        config.character_set,
        config.max_component_len,
    );
    let handle = sanitise_component(&clip.handle, config.character_set, config.max_component_len);
    let album = derive_album(clip, request.playlist_title, config.album_mode)
        .map(|value| sanitise_component(&value, config.character_set, config.max_component_len))
        .unwrap_or_default();
    let title = sanitise_component(
        &title_name(clip),
        config.character_set,
        config.max_component_len,
    );
    let id = sanitise_component(&clip.id, CharacterSet::Ascii, config.max_component_len);
    let mut components = config
        .template
        .split('/')
        .filter_map(|segment| {
            let rendered = segment
                .replace("{creator}", &creator)
                .replace("{handle}", &handle)
                .replace("{album}", &album)
                .replace("{title}", &title)
                .replace("{id}", &id);
            let sanitised =
                sanitise_component(&rendered, config.character_set, config.max_component_len);
            (!sanitised.is_empty()).then_some(sanitised)
        })
        .collect::<Vec<_>>();

    if components.is_empty() {
        components.push(title.clone());
    }

    let base_name = components
        .pop()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| title.clone());
    let mut relative_path = PathBuf::new();

    for component in components {
        relative_path.push(component);
    }

    let base_name = if needs_untitled_suffix(clip, &title) {
        append_suffix(&base_name, short_id(clip), config.max_component_len)
    } else {
        base_name
    };

    relative_path.push(&base_name);
    RenderedName {
        relative_path,
        base_name,
    }
}

fn with_suffix(mut rendered: RenderedName, suffix: &str, max_component_len: usize) -> RenderedName {
    rendered.base_name = append_suffix(&rendered.base_name, suffix, max_component_len);
    rendered.relative_path.set_file_name(&rendered.base_name);
    rendered
}

fn lineage_album(clip: &Clip) -> Option<String> {
    non_blank(&clip.album_title)
        .map(str::to_string)
        .or_else(|| {
            let root = non_blank(&clip.root_ancestor_id)?;
            (root != clip.id).then(|| root.to_string())
        })
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

fn needs_untitled_suffix(clip: &Clip, rendered_title: &str) -> bool {
    clip.title.trim().is_empty()
        || clip.title.trim().eq_ignore_ascii_case("untitled")
        || rendered_title.is_empty()
}

fn append_suffix(base: &str, suffix: &str, max_component_len: usize) -> String {
    let suffix = format!(" [{suffix}]");
    let max_len = max_component_len.max(suffix.chars().count() + 1);
    let allowed = max_len.saturating_sub(suffix.chars().count());
    let truncated = truncate_chars(base.trim_end(), allowed);
    let combined = format!("{truncated}{suffix}");
    sanitise_component(&combined, CharacterSet::Unicode, max_len)
}

fn sanitise_component(
    value: &str,
    character_set: CharacterSet,
    max_component_len: usize,
) -> String {
    let filtered = match character_set {
        CharacterSet::Unicode => value.chars().map(unicode_char).collect::<String>(),
        CharacterSet::Ascii => value.chars().flat_map(ascii_chars).collect::<String>(),
    };
    let collapsed = filtered.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches([' ', '.']);
    if trimmed.is_empty() {
        return String::new();
    }

    let mut result = truncate_chars(trimmed, max_component_len.max(1));
    result = result.trim_matches([' ', '.']).to_string();
    if result.is_empty() {
        return String::new();
    }
    if result == "." || result == ".." {
        return "item".to_string();
    }
    if is_reserved_name(&result) {
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
        _ if ch.is_whitespace() => vec![' '],
        _ => vec![' '],
    }
}

fn truncate_chars(value: &str, max_len: usize) -> String {
    value.chars().take(max_len).collect()
}

fn short_id(clip: &Clip) -> &str {
    let end = clip
        .id
        .char_indices()
        .nth(8)
        .map_or(clip.id.len(), |(index, _)| index);
    &clip.id[..end]
}

fn non_blank(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn is_reserved_name(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn clip(id: &str, title: &str) -> Clip {
        Clip {
            id: id.to_string(),
            title: title.to_string(),
            display_name: "München".to_string(),
            handle: "munchen".to_string(),
            album_title: String::new(),
            root_ancestor_id: String::new(),
            ..Clip::default()
        }
    }

    #[test]
    fn unicode_names_are_preserved_and_ascii_falls_back() {
        let clip = clip("abc12345", "Beyoncé/東京");

        let unicode = render_clip_name(
            NamingRequest {
                clip: &clip,
                playlist_title: None,
            },
            &NamingConfig::default(),
        );
        assert_eq!(
            unicode.relative_path.to_string_lossy(),
            "München/Beyoncé 東京"
        );

        let ascii = render_clip_name(
            NamingRequest {
                clip: &clip,
                playlist_title: None,
            },
            &NamingConfig {
                character_set: CharacterSet::Ascii,
                ..NamingConfig::default()
            },
        );
        assert_eq!(ascii.relative_path.to_string_lossy(), "Munchen/Beyonce");
    }

    #[test]
    fn reserved_and_hostile_names_are_sanitised() {
        let clip = Clip {
            id: "deadbeef".to_string(),
            title: "CON<>:\"/\\|?*.".to_string(),
            display_name: "AUX".to_string(),
            ..Clip::default()
        };

        let rendered = render_clip_name(
            NamingRequest {
                clip: &clip,
                playlist_title: None,
            },
            &NamingConfig::default(),
        );
        assert_eq!(rendered.relative_path.to_string_lossy(), "AUX_/CON_");
    }

    #[test]
    fn blank_titles_use_a_stable_suffix() {
        let clip = clip("12345678-clip", "   ");

        let rendered = render_clip_name(
            NamingRequest {
                clip: &clip,
                playlist_title: None,
            },
            &NamingConfig::default(),
        );
        assert_eq!(rendered.base_name, "Untitled [12345678]");
        assert_eq!(
            rendered.relative_path.to_string_lossy(),
            "München/Untitled [12345678]"
        );
    }

    #[test]
    fn very_long_titles_are_trimmed() {
        let clip = clip("abcdef12", &"a".repeat(120));
        let rendered = render_clip_name(
            NamingRequest {
                clip: &clip,
                playlist_title: None,
            },
            &NamingConfig {
                max_component_len: 24,
                ..NamingConfig::default()
            },
        );

        assert_eq!(rendered.base_name.chars().count(), 24);
        assert!(rendered.base_name.chars().all(|ch| ch == 'a'));
    }

    #[test]
    fn duplicate_titles_get_deterministic_suffixes() {
        let first = clip("11111111-alpha", "Shared");
        let second = clip("22222222-beta", "Shared");
        let requests = [
            NamingRequest {
                clip: &first,
                playlist_title: None,
            },
            NamingRequest {
                clip: &second,
                playlist_title: None,
            },
        ];
        let swapped = [
            NamingRequest {
                clip: &second,
                playlist_title: None,
            },
            NamingRequest {
                clip: &first,
                playlist_title: None,
            },
        ];

        let names = render_clip_names(&requests, &NamingConfig::default());
        let swapped_names = render_clip_names(&swapped, &NamingConfig::default());

        assert_eq!(
            names[0].relative_path.to_string_lossy(),
            "München/Shared [11111111]"
        );
        assert_eq!(
            names[1].relative_path.to_string_lossy(),
            "München/Shared [22222222]"
        );

        let by_id = requests
            .iter()
            .zip(names.iter())
            .map(|(request, name)| {
                (
                    request.clip.id.clone(),
                    name.relative_path.to_string_lossy().into_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let swapped_by_id = swapped
            .iter()
            .zip(swapped_names.iter())
            .map(|(request, name)| {
                (
                    request.clip.id.clone(),
                    name.relative_path.to_string_lossy().into_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(by_id, swapped_by_id);
    }

    #[test]
    fn lineage_album_uses_album_title_then_root_ancestor() {
        let root = Clip {
            id: "root".to_string(),
            title: "Original".to_string(),
            ..Clip::default()
        };
        let child = Clip {
            id: "child".to_string(),
            title: "Remix".to_string(),
            root_ancestor_id: "root".to_string(),
            ..Clip::default()
        };
        let album = Clip {
            id: "album".to_string(),
            title: "Track".to_string(),
            album_title: "Weather Series".to_string(),
            root_ancestor_id: "root".to_string(),
            ..Clip::default()
        };

        assert_eq!(derive_album(&root, None, AlbumMode::Lineage), None);
        assert_eq!(
            derive_album(&child, None, AlbumMode::Lineage).as_deref(),
            Some("root")
        );
        assert_eq!(
            derive_album(&album, None, AlbumMode::Lineage).as_deref(),
            Some("Weather Series")
        );
    }

    #[test]
    fn playlist_album_mode_prefers_the_playlist_title() {
        let clip = Clip {
            id: "clip".to_string(),
            title: "Track".to_string(),
            album_title: "Lineage Album".to_string(),
            ..Clip::default()
        };

        assert_eq!(
            derive_album(&clip, Some("Road Trip"), AlbumMode::Playlist).as_deref(),
            Some("Road Trip")
        );
        assert_eq!(
            derive_album(&clip, Some("   "), AlbumMode::Playlist).as_deref(),
            Some("Lineage Album")
        );
    }
}
