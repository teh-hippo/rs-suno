//! FLAC metadata plumbing behind the surgical retag: an owned metadata-block
//! walker and `VORBIS_COMMENT` codec.
//!
//! [`metaflac`](metaflac) models the comments as a `HashMap`, so writing a tag
//! back reorders every entry even when nothing changed (#537). This module
//! parses the blocks itself and edits in place: the vendor string, duplicate
//! keys, key casing, and the order of untouched entries all survive, every
//! non-comment block keeps its exact bytes, and the audio frames are copied
//! across verbatim.
//!
//! Everything here slices with `get`, so malformed or truncated input returns
//! an [`Error::Tag`] rather than panicking the way `metaflac` does.

use std::borrow::Cow;

use crate::error::{Error, Result};
use crate::tag::{Cover, FLAC_METADATA_BLOCK_MAX, TrackMetadata, flac_picture_data_budget};

/// The FLAC stream marker every file opens with.
const FLAC_MAGIC: &[u8; 4] = b"fLaC";

/// `STREAMINFO`, which must be the first metadata block and is always 34 bytes.
const BLOCK_STREAMINFO: u8 = 0;
/// `PADDING`, conventionally the last metadata block.
const BLOCK_PADDING: u8 = 1;
/// `VORBIS_COMMENT`, the block this module edits.
const BLOCK_VORBIS_COMMENT: u8 = 4;
/// `PICTURE`, holding one embedded image each.
const BLOCK_PICTURE: u8 = 6;
/// Block type 127 is invalid per the FLAC specification.
const BLOCK_INVALID: u8 = 127;

/// The fixed `STREAMINFO` body length.
const STREAMINFO_LEN: usize = 34;

/// The `PICTURE` API picture type for a front cover.
const PICTURE_TYPE_FRONT_COVER: u32 = 3;

/// The comment keys this crate writes and therefore owns on a retag. Everything
/// else in the block belongs to the user (or another tool) and is left alone.
const OWNED_TRACK_KEYS: [&str; 2] = ["TRACKNUMBER", "TRACKTOTAL"];

/// One FLAC metadata block: its type plus its body, borrowed from the input
/// until something replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataBlock<'a> {
    block_type: u8,
    body: Cow<'a, [u8]>,
}

/// A parsed FLAC stream: its metadata blocks and the audio frames that follow.
#[derive(Debug, Clone)]
struct FlacFile<'a> {
    blocks: Vec<MetadataBlock<'a>>,
    frames: &'a [u8],
}

impl<'a> FlacFile<'a> {
    /// Split `audio` into its metadata blocks and audio frames.
    ///
    /// Errors on a missing `fLaC` marker, a first block that is not a 34-byte
    /// `STREAMINFO`, an invalid block type, or a truncated header or body.
    fn parse(audio: &'a [u8]) -> Result<Self> {
        let rest = audio
            .strip_prefix(FLAC_MAGIC)
            .ok_or_else(|| malformed("missing the fLaC stream marker"))?;

        let mut blocks: Vec<MetadataBlock<'a>> = Vec::new();
        let mut offset = 0usize;
        loop {
            let header = rest
                .get(offset..offset + 4)
                .ok_or_else(|| malformed("a metadata block header is truncated"))?;
            let is_last = header[0] & 0x80 != 0;
            let block_type = header[0] & 0x7f;
            if block_type == BLOCK_INVALID {
                return Err(malformed("an invalid metadata block type"));
            }
            let len = usize::try_from(u32::from_be_bytes([0, header[1], header[2], header[3]]))
                .map_err(|_| malformed("a metadata block length is out of range"))?;
            let start = offset + 4;
            let end = start
                .checked_add(len)
                .ok_or_else(|| malformed("a metadata block length overflows"))?;
            let body = rest
                .get(start..end)
                .ok_or_else(|| malformed("a metadata block body is truncated"))?;
            if blocks.is_empty() && (block_type != BLOCK_STREAMINFO || len != STREAMINFO_LEN) {
                return Err(malformed("the first block is not a 34-byte STREAMINFO"));
            }
            blocks.push(MetadataBlock {
                block_type,
                body: Cow::Borrowed(body),
            });
            offset = end;
            if is_last {
                break;
            }
        }

        Ok(Self {
            blocks,
            frames: &rest[offset..],
        })
    }

    /// Serialise the blocks and frames back into a FLAC stream, stamping the
    /// last-block flag on the final block and refusing a body over the 24-bit
    /// block length `metaflac` would silently truncate.
    fn encode(&self) -> Result<Vec<u8>> {
        let last = self
            .blocks
            .len()
            .checked_sub(1)
            .ok_or_else(|| malformed("no metadata blocks to write"))?;
        let mut out = Vec::with_capacity(self.frames.len() + 1024);
        out.extend_from_slice(FLAC_MAGIC);
        for (index, block) in self.blocks.iter().enumerate() {
            if block.body.len() > FLAC_METADATA_BLOCK_MAX {
                return Err(Error::Tag(format!(
                    "a FLAC metadata block is {} bytes, over the {FLAC_METADATA_BLOCK_MAX}-byte limit",
                    block.body.len()
                )));
            }
            let mut head = block.block_type & 0x7f;
            if index == last {
                head |= 0x80;
            }
            out.push(head);
            let len = u32::try_from(block.body.len())
                .map_err(|_| malformed("a metadata block length is out of range"))?;
            out.extend_from_slice(&len.to_be_bytes()[1..]);
            out.extend_from_slice(&block.body);
        }
        out.extend_from_slice(self.frames);
        Ok(out)
    }

    /// The index of the first `VORBIS_COMMENT` block, if the file has one.
    ///
    /// A conforming FLAC holds at most one; any further comment block is left
    /// untouched rather than merged, so a malformed file is never made worse.
    fn comment_index(&self) -> Option<usize> {
        self.blocks
            .iter()
            .position(|block| block.block_type == BLOCK_VORBIS_COMMENT)
    }

    /// The index of the first front-cover `PICTURE` block, if any. Pictures of
    /// other types (back cover, artist, and so on) are not ours to touch.
    fn front_cover_index(&self) -> Option<usize> {
        self.blocks.iter().position(|block| {
            block.block_type == BLOCK_PICTURE
                && picture_type(&block.body) == Some(PICTURE_TYPE_FRONT_COVER)
        })
    }

    /// Where a newly added block belongs: before a trailing `PADDING` block
    /// when there is one, else at the end.
    fn append_index(&self) -> usize {
        match self.blocks.last() {
            Some(block) if block.block_type == BLOCK_PADDING => self.blocks.len() - 1,
            _ => self.blocks.len(),
        }
    }
}

/// The `PICTURE` API type of a block body, or `None` when it is too short.
fn picture_type(body: &[u8]) -> Option<u32> {
    body.get(0..4)
        .map(|head| u32::from_be_bytes([head[0], head[1], head[2], head[3]]))
}

/// A `VORBIS_COMMENT` block body, kept as raw bytes so nothing is normalised.
///
/// Entries stay in file order, with their original `KEY=value` bytes, so a
/// duplicate key, an unusual casing, or a non-UTF-8 comment written by another
/// tool all round-trip untouched. `trailer` captures any bytes after the last
/// entry (some encoders pad the block) so a no-op re-encode is byte-identical.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct VorbisComments {
    vendor: Vec<u8>,
    entries: Vec<Vec<u8>>,
    trailer: Vec<u8>,
}

impl VorbisComments {
    /// Parse a `VORBIS_COMMENT` block body, erroring on truncation.
    fn parse(body: &[u8]) -> Result<Self> {
        let mut cur = body;
        let vendor = take_counted(&mut cur)?.to_vec();
        let count = read_u32_le(&mut cur)?;
        let mut entries = Vec::new();
        // Read one entry at a time rather than reserving `count` up front: a
        // corrupt count of four billion must fail on the first short read, not
        // allocate.
        for _ in 0..count {
            entries.push(take_counted(&mut cur)?.to_vec());
        }
        Ok(Self {
            vendor,
            entries,
            trailer: cur.to_vec(),
        })
    }

    /// Serialise back into a block body.
    fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        push_counted(&mut out, &self.vendor)?;
        let count =
            u32::try_from(self.entries.len()).map_err(|_| malformed("too many comment entries"))?;
        out.extend_from_slice(&count.to_le_bytes());
        for entry in &self.entries {
            push_counted(&mut out, entry)?;
        }
        out.extend_from_slice(&self.trailer);
        Ok(out)
    }

    /// Nothing to write: no vendor string, no entries, no trailer.
    fn is_empty(&self) -> bool {
        self.vendor.is_empty() && self.entries.is_empty() && self.trailer.is_empty()
    }

    /// Set `key` to `value`, matching the key case-insensitively.
    ///
    /// An existing entry is rewritten where it sits, keeping its own key
    /// spelling and its position in the block, and any later duplicate of that
    /// key is dropped. A key that is not present yet is appended, so repeated
    /// runs append in a fixed order.
    fn set(&mut self, key: &str, value: &str) {
        let mut first: Option<usize> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry_key(entry).eq_ignore_ascii_case(key.as_bytes()) {
                first = Some(index);
                break;
            }
        }
        match first {
            Some(index) => {
                let existing_key = entry_key(&self.entries[index]).to_vec();
                self.entries[index] = join_entry(&existing_key, value.as_bytes());
                let mut seen = false;
                self.entries.retain(|entry| {
                    if entry_key(entry).eq_ignore_ascii_case(key.as_bytes()) {
                        let keep = !seen;
                        seen = true;
                        keep
                    } else {
                        true
                    }
                });
            }
            None => self
                .entries
                .push(join_entry(key.as_bytes(), value.as_bytes())),
        }
    }

    /// Remove every entry whose key matches `key`, case-insensitively.
    fn remove(&mut self, key: &str) {
        self.entries
            .retain(|entry| !entry_key(entry).eq_ignore_ascii_case(key.as_bytes()));
    }

    /// Set `key` when `value` has content, else remove it: an owned field with
    /// no value is absent from a freshly tagged file, so a retag drops it.
    fn set_or_remove(&mut self, key: &str, value: &str) {
        if value.is_empty() {
            self.remove(key);
        } else {
            self.set(key, value);
        }
    }

    /// The values held under `key`, in file order (test and caller helper).
    #[cfg(test)]
    fn values(&self, key: &str) -> Vec<&[u8]> {
        self.entries
            .iter()
            .filter(|entry| entry_key(entry).eq_ignore_ascii_case(key.as_bytes()))
            .map(|entry| match entry.iter().position(|byte| *byte == b'=') {
                Some(at) => &entry[at + 1..],
                None => &[][..],
            })
            .collect()
    }
}

/// The key part of a `KEY=value` entry: the bytes before the first `=`, or the
/// whole entry when it has none (malformed, but preserved rather than dropped).
fn entry_key(entry: &[u8]) -> &[u8] {
    match entry.iter().position(|byte| *byte == b'=') {
        Some(at) => &entry[..at],
        None => entry,
    }
}

/// Build a `KEY=value` entry from its parts.
fn join_entry(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 1 + value.len());
    out.extend_from_slice(key);
    out.push(b'=');
    out.extend_from_slice(value);
    out
}

/// Read a little-endian `u32`, advancing `cur`.
fn read_u32_le(cur: &mut &[u8]) -> Result<u32> {
    let head = cur
        .get(0..4)
        .ok_or_else(|| malformed("the comment block is truncated"))?;
    *cur = &cur[4..];
    Ok(u32::from_le_bytes([head[0], head[1], head[2], head[3]]))
}

/// Read a length-prefixed byte run, advancing `cur`.
fn take_counted<'a>(cur: &mut &'a [u8]) -> Result<&'a [u8]> {
    let len = usize::try_from(read_u32_le(cur)?)
        .map_err(|_| malformed("a comment length is out of range"))?;
    let taken = cur
        .get(0..len)
        .ok_or_else(|| malformed("the comment block is truncated"))?;
    *cur = &cur[len..];
    Ok(taken)
}

/// Append a length-prefixed byte run.
fn push_counted(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| malformed("a comment is too long"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// A malformed-input error, worded so it never echoes the input bytes.
fn malformed(what: &str) -> Error {
    Error::Tag(format!("could not read FLAC metadata: {what}"))
}

/// Serialise a FLAC `PICTURE` block body for `cover` as a front cover.
///
/// The dimensions, colour depth, and colour count are left at zero, exactly as
/// `metaflac` writes them on the fresh path, so the two paths agree byte for
/// byte on the same cover.
fn picture_body(cover: Cover<'_>) -> Vec<u8> {
    let mime = cover.mime.as_bytes();
    let mut out = Vec::with_capacity(mime.len() + cover.bytes.len() + 32);
    out.extend_from_slice(&PICTURE_TYPE_FRONT_COVER.to_be_bytes());
    out.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    out.extend_from_slice(mime);
    // Empty description, then width, height, depth, and colour count.
    for _ in 0..5 {
        out.extend_from_slice(&0u32.to_be_bytes());
    }
    out.extend_from_slice(&(cover.bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(cover.bytes);
    out
}

/// Retag an existing FLAC in place, touching only the fields this crate owns.
///
/// Unlike [`tag_flac`](crate::tag_flac), which rebuilds the whole comment block
/// for a freshly downloaded file, this edits the parsed block: owned keys are
/// replaced where they sit or appended when new, owned keys with no value are
/// removed, and every other comment, the vendor string, duplicate keys, and key
/// casing survive untouched. All non-comment blocks and the audio frames keep
/// their exact bytes.
///
/// A `cover` replaces the front-cover `PICTURE` block only; pictures of other
/// types are never touched. With no `cover`, `preserve_existing_cover` keeps the
/// existing front cover, and clearing it removes the front cover the way a fresh
/// tag of a clip with no art would leave the file.
///
/// When the resulting metadata is byte-identical to what is already there, the
/// original bytes are returned unchanged, so a semantic no-op never rewrites the
/// file (#537).
pub fn retag_flac(
    audio: &[u8],
    meta: &TrackMetadata,
    cover: Option<Cover<'_>>,
    preserve_existing_cover: bool,
) -> Result<Vec<u8>> {
    let mut file = FlacFile::parse(audio)?;
    let original = file.blocks.clone();

    let comment_index = file.comment_index();
    let mut comments = match comment_index {
        Some(index) => VorbisComments::parse(&file.blocks[index].body)?,
        None => VorbisComments::default(),
    };
    apply_owned_comments(&mut comments, meta);

    match comment_index {
        Some(index) => file.blocks[index].body = Cow::Owned(comments.encode()?),
        None => {
            if !comments.is_empty() {
                // A conforming file puts the comments straight after STREAMINFO.
                file.blocks.insert(
                    1.min(file.blocks.len()),
                    MetadataBlock {
                        block_type: BLOCK_VORBIS_COMMENT,
                        body: Cow::Owned(comments.encode()?),
                    },
                );
            }
        }
    }

    apply_owned_cover(&mut file, cover, preserve_existing_cover)?;

    if file.blocks == original {
        return Ok(audio.to_vec());
    }
    file.encode()
}

/// Write this crate's fields into `comments`, leaving everything else alone.
///
/// `LYRICS` is the one exception to "empty means remove": a retag that carries
/// no lyrics keeps whatever is embedded, matching the fresh path, so a run that
/// simply has no lyrics to hand never strips a good file.
fn apply_owned_comments(comments: &mut VorbisComments, meta: &TrackMetadata) {
    for (key, value) in meta.standard_fields().into_iter().chain(meta.suno_fields()) {
        if key == "LYRICS" && value.is_empty() {
            continue;
        }
        comments.set_or_remove(key, value);
    }
    if meta.track > 0 {
        comments.set("TRACKNUMBER", &meta.track.to_string());
        if meta.track_total > 0 {
            comments.set("TRACKTOTAL", &meta.track_total.to_string());
        } else {
            comments.remove("TRACKTOTAL");
        }
    } else {
        for key in OWNED_TRACK_KEYS {
            comments.remove(key);
        }
    }
}

/// Apply the front-cover policy to the parsed blocks.
fn apply_owned_cover(
    file: &mut FlacFile<'_>,
    cover: Option<Cover<'_>>,
    preserve_existing_cover: bool,
) -> Result<()> {
    let Some(cover) = cover else {
        if !preserve_existing_cover && let Some(index) = file.front_cover_index() {
            file.blocks.remove(index);
        }
        return Ok(());
    };

    let budget = flac_picture_data_budget(cover.mime);
    if cover.bytes.len() > budget {
        return Err(Error::Tag(format!(
            "cover image is {} bytes, over the {}-byte FLAC picture limit",
            cover.bytes.len(),
            budget
        )));
    }
    let block = MetadataBlock {
        block_type: BLOCK_PICTURE,
        body: Cow::Owned(picture_body(cover)),
    };
    match file.front_cover_index() {
        Some(index) => file.blocks[index] = block,
        None => {
            let at = file.append_index();
            file.blocks.insert(at, block);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A comment block body with the given vendor and entries.
    fn comment_body(vendor: &str, entries: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        out.extend_from_slice(vendor.as_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for entry in entries {
            out.extend_from_slice(&(entry.len() as u32).to_le_bytes());
            out.extend_from_slice(entry.as_bytes());
        }
        out
    }

    #[test]
    fn round_trips_vendor_order_and_duplicates() {
        let body = comment_body(
            "reference libFLAC 1.4.3",
            &["title=Kept", "ARTIST=A", "artist=B", "MOOD=calm"],
        );
        let comments = VorbisComments::parse(&body).unwrap();
        assert_eq!(comments.vendor, b"reference libFLAC 1.4.3");
        assert_eq!(comments.entries.len(), 4);
        assert_eq!(comments.encode().unwrap(), body, "re-encode is byte-exact");
    }

    #[test]
    fn set_replaces_in_place_keeping_case_and_order() {
        let body = comment_body("v", &["title=Old", "MOOD=calm", "ARTIST=A"]);
        let mut comments = VorbisComments::parse(&body).unwrap();
        comments.set("TITLE", "New");
        assert_eq!(
            comments.entries,
            vec![
                b"title=New".to_vec(),
                b"MOOD=calm".to_vec(),
                b"ARTIST=A".to_vec()
            ],
            "the existing key spelling and position are kept"
        );
    }

    #[test]
    fn set_collapses_duplicates_of_an_owned_key() {
        let body = comment_body("v", &["TITLE=One", "MOOD=calm", "title=Two"]);
        let mut comments = VorbisComments::parse(&body).unwrap();
        comments.set("TITLE", "Only");
        assert_eq!(
            comments.entries,
            vec![b"TITLE=Only".to_vec(), b"MOOD=calm".to_vec()]
        );
    }

    #[test]
    fn set_appends_a_missing_key() {
        let body = comment_body("v", &["MOOD=calm"]);
        let mut comments = VorbisComments::parse(&body).unwrap();
        comments.set("LYRICS", "la la");
        assert_eq!(comments.values("LYRICS"), vec![b"la la".as_slice()]);
        assert_eq!(comments.entries[0], b"MOOD=calm".to_vec());
    }

    #[test]
    fn remove_is_case_insensitive_and_leaves_others() {
        let body = comment_body("v", &["Title=One", "MOOD=calm", "TITLE=Two"]);
        let mut comments = VorbisComments::parse(&body).unwrap();
        comments.remove("title");
        assert_eq!(comments.entries, vec![b"MOOD=calm".to_vec()]);
    }

    #[test]
    fn preserves_a_non_utf8_and_keyless_entry() {
        // Another tool's malformed entries must survive a retag untouched.
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&2u32.to_le_bytes());
        let odd: &[u8] = b"BROKEN\xFF\xFE";
        body.extend_from_slice(&(odd.len() as u32).to_le_bytes());
        body.extend_from_slice(odd);
        let keyless: &[u8] = b"no-equals-sign";
        body.extend_from_slice(&(keyless.len() as u32).to_le_bytes());
        body.extend_from_slice(keyless);

        let mut comments = VorbisComments::parse(&body).unwrap();
        comments.set("TITLE", "x");
        assert_eq!(comments.entries[0], odd.to_vec());
        assert_eq!(comments.entries[1], keyless.to_vec());
    }

    #[test]
    fn truncated_comment_bodies_error_rather_than_panic() {
        assert!(VorbisComments::parse(&[]).is_err());
        assert!(VorbisComments::parse(&[0, 0, 0]).is_err());
        // A vendor length longer than the body.
        assert!(VorbisComments::parse(&[8, 0, 0, 0, b'a']).is_err());
        // A count of four billion with no entries behind it.
        let mut body = 0u32.to_le_bytes().to_vec();
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(VorbisComments::parse(&body).is_err());
    }

    #[test]
    fn trailing_block_bytes_survive_a_round_trip() {
        let mut body = comment_body("v", &["MOOD=calm"]);
        body.extend_from_slice(b"\x01padding");
        let comments = VorbisComments::parse(&body).unwrap();
        assert_eq!(comments.trailer, b"\x01padding");
        assert_eq!(comments.encode().unwrap(), body);
    }

    #[test]
    fn parse_rejects_malformed_streams() {
        assert!(FlacFile::parse(b"").is_err());
        assert!(FlacFile::parse(b"not a flac file at all").is_err());
        // Marker only, no block header.
        assert!(FlacFile::parse(b"fLaC").is_err());
        // A STREAMINFO header promising 34 bytes that are not there.
        assert!(FlacFile::parse(b"fLaC\x80\x00\x00\x22short").is_err());
        // A first block that is not STREAMINFO.
        let mut wrong = b"fLaC".to_vec();
        wrong.push(0x84);
        wrong.extend_from_slice(&[0x00, 0x00, 0x00]);
        assert!(FlacFile::parse(&wrong).is_err());
    }

    #[test]
    fn picture_type_reads_the_leading_field() {
        assert_eq!(
            picture_type(&PICTURE_TYPE_FRONT_COVER.to_be_bytes()),
            Some(3)
        );
        assert_eq!(picture_type(b"ab"), None);
    }

    #[test]
    fn encode_rejects_an_oversized_block() {
        let file = FlacFile {
            blocks: vec![MetadataBlock {
                block_type: BLOCK_STREAMINFO,
                body: Cow::Owned(vec![0u8; FLAC_METADATA_BLOCK_MAX + 1]),
            }],
            frames: &[],
        };
        assert!(matches!(file.encode(), Err(Error::Tag(_))));
    }
}
