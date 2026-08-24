use anyhow::{Context, Ok, Result, anyhow};
use aws_lc_rs::digest::{Context as DigestContext, SHA256};
use indicatif::MultiProgress;
use osm_pbf_iter::{Blob, Primitive, PrimitiveBlock, RelationMemberType};
use protobuf_iter::MessageIter;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;
use time::UtcDateTime;
use time::format_description::well_known::Rfc3339;

use crate::make_download_bar;
use crate::tables::OsmFeatures;
use crate::utils::to_hex;

mod assemble;
mod fetch;
mod id_tagging_schema;
mod index;
mod prune;

use prune::Prunings;

/// Encodes an OpenStreetMap element's `(type, id)` as a `Feature.id` --
/// `id * 10 + 1` for nodes, `+ 2` for ways, `+ 3` for relations. The same
/// encoding is also used for relation member references
/// (`RelationMember.id`) -- see `feature.proto`. Inverse of
/// [`decode_feature_id`].
pub(crate) fn encode_feature_id(member_type: RelationMemberType, id: u64) -> u64 {
    match member_type {
        RelationMemberType::Node => id * 10 + 1,
        RelationMemberType::Way => id * 10 + 2,
        RelationMemberType::Relation => id * 10 + 3,
    }
}

/// Decodes a `Feature.id`-encoded value into `(type, id)` -- `None` if
/// `fid`'s last decimal digit isn't 1/2/3, which should never happen for
/// a value this pipeline produced itself. Inverse of
/// [`encode_feature_id`].
pub(crate) fn decode_feature_id(fid: u64) -> Option<(RelationMemberType, u64)> {
    let id = fid / 10;
    match fid % 10 {
        1 => Some((RelationMemberType::Node, id)),
        2 => Some((RelationMemberType::Way, id)),
        3 => Some((RelationMemberType::Relation, id)),
        _ => None,
    }
}

/// `"node"`/`"way"`/`"relation"` -- the string OSM's own API and tag
/// conventions use for this element type (e.g. `conflated.parquet`'s
/// `osm.type` column).
pub(crate) fn osm_type_str(member_type: RelationMemberType) -> &'static str {
    match member_type {
        RelationMemberType::Node => "node",
        RelationMemberType::Way => "way",
        RelationMemberType::Relation => "relation",
    }
}

pub fn import_osm<'a>(
    http_client: &reqwest::Client,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<OsmFeatures<'a>> {
    assert!(workdir.exists());

    let osm_index_path = workdir.join("osm-features.index");
    let strings_path = assemble::strings_path(workdir);
    if OsmFeatures::exists(&osm_index_path, &strings_path) {
        return OsmFeatures::open(&osm_index_path, &strings_path);
    }

    // Each sub-step below gets the same step/phase/elapsed_seconds/
    // memstats logging shape as this crate's top-level pipeline steps
    // (see `super::run_step`, which this reuses directly rather than a
    // second, parallel logging mechanism) -- just under a dotted name
    // ("import_osm.fetch", not "fetch") so a log consumer can tell a
    // sub-step from a top-level one at a glance, and so `import_osm`'s
    // own already-existing top-level start/end pair (logged by
    // whichever `run_step` call wraps this whole function, see
    // `pipeline::run_pipeline_steps`) keeps meaning "the whole step",
    // not "the whole step minus its sub-steps". Before this, only two
    // points inside `import_osm` were distinguishable at all --
    // "opened OpenStreetMap planet file" (after fetch+hash+blob-scan)
    // and the outer step's own end -- which lumps prune/assemble/
    // index-build into one undifferentiated number; not enough
    // resolution to tell which of those actually needs the memory a
    // tight `--mem-limit` run runs short on (see #711's investigation,
    // e.g. alltheplaces/osm-diffs#711's comments for a real case where
    // that distinction mattered).
    let (pbf, fetch_metadata) = super::run_step("import_osm.fetch", || {
        fetch::fetch_planet(http_client, progress, workdir)
    })?;
    let pbf_error = || format!("could not open file `{:?}`", pbf);
    let mut file = File::open(&pbf).with_context(pbf_error)?;
    let mut reader = super::run_step("import_osm.open", || {
        BlobReader::open(&mut file).with_context(pbf_error)
    })?;
    let header = reader.header();
    let replication_timestamp = header
        .replication_timestamp
        .format(&Rfc3339)
        .expect("UtcDateTime should always format as RFC3339");
    log::info!(
        replication_timestamp = replication_timestamp.as_str(),
        source = header.source.as_deref(),
        writing_program = header.writing_program.as_deref(),
        sha256 = fetch_metadata.sha256.as_deref();
        "opened OpenStreetMap planet file"
    );

    let prunings = super::run_step("import_osm.prune", || {
        Prunings::create(&mut reader, progress, workdir)
    })?;
    let assembly = super::run_step("import_osm.assemble", || {
        assemble::assemble(&mut reader, &prunings, progress, workdir)
    })?;
    let index = super::run_step("import_osm.index", || {
        index::build_index(&assembly, progress, workdir, &osm_index_path)
    })?;
    Ok(OsmFeatures {
        index,
        strings: assembly.strings,
    })
}

/// Filename, within `workdir`, that `fetch::fetch_planet` downloads the
/// OSM planet PBF to. Shared with `pipeline::provenance`, which needs to
/// find it again (via `read_header`) without re-fetching. Matches
/// upstream's own name for this file (see `OSM_TORRENT_URL` in
/// `fetch.rs`), rather than inventing a local one.
pub(crate) const PLANET_PBF_FILENAME: &str = "planet-latest.osm.pbf";

/// Provenance metadata read from a PBF file's `OSMHeader` block: which
/// OpenStreetMap replication state the data corresponds to, and what
/// produced the file. Lets us embed the provenance of our input data
/// into our output files.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OsmMetadata {
    #[serde(with = "crate::utils::rfc3339")]
    pub replication_timestamp: UtcDateTime,
    pub source: Option<String>,
    pub writing_program: Option<String>,

    /// SHA-256 of the downloaded planet file, as lowercase hex --
    /// computed by us, not reported by OpenStreetMap. `None` for a
    /// value built by `parse_header_block`/`read_header` (which only
    /// ever reads the small `OSMHeader` block, not the rest of the
    /// file -- there's no hash to give); `Some` only for the value
    /// `compute_and_persist_metadata` builds after actually hashing the
    /// whole file. Same shape as `AtpMetadata::sha256`, for an
    /// analogous reason: this struct is used both before and after the
    /// point where a hash becomes available.
    pub sha256: Option<String>,
}

/// Reads just the `OSMHeader` block from the very start of a PBF file --
/// a few hundred bytes -- without scanning the rest of the file for data
/// blobs the way `BlobReader::open()` does. Cheap enough to call on
/// demand wherever `OsmMetadata` is needed (e.g. when assembling this
/// pipeline's provenance BOM), independent of whatever `BlobReader`'s
/// callers do with the rest of the file.
///
/// Every `.osm.pbf` file starts with exactly one `OSMHeader` blob, so
/// this only ever reads the first blob; if that's not `OSMHeader`, it
/// errors out rather than scanning further for one.
pub fn read_header(path: &Path) -> Result<OsmMetadata> {
    let mut file = File::open(path).with_context(|| format!("could not open file `{:?}`", path))?;
    let blob_header = BlobReader::<File>::read_blob_header(&mut file)?;
    let Some((blob_type, data_size)) = BlobReader::<File>::parse_blob_header(&blob_header) else {
        return Err(anyhow!("bad blob header at start of `{:?}`", path));
    };
    if blob_type != b"OSMHeader" {
        return Err(anyhow!(
            "expected an OSMHeader blob at the start of `{:?}`, found {:?}",
            path,
            String::from_utf8_lossy(blob_type)
        ));
    }
    let offset = 4_u64 + (blob_header.len() as u64);
    let blob = BlobReader::<File>::read_blob(&mut file, offset, data_size)?;
    BlobReader::<File>::parse_header_block(&blob.into_data())
}

/// Filename, within `workdir`, that [`compute_and_persist_metadata`]
/// persists a fully-populated [`OsmMetadata`] (including `sha256`) to.
/// Derived from [`PLANET_PBF_FILENAME`] rather than a separate literal,
/// so it can't drift if that ever changes again (see #648).
fn meta_json_path(workdir: &Path) -> PathBuf {
    workdir.join(format!("{PLANET_PBF_FILENAME}.meta.json"))
}

/// Reads back the [`OsmMetadata`] persisted for a prior
/// `fetch::fetch_planet` call in `workdir`.
pub(crate) fn read_cached_metadata(workdir: &Path) -> Result<OsmMetadata> {
    let path = meta_json_path(workdir);
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("Failed to parse {}", path.display()))
}

/// Computes a fully-populated [`OsmMetadata`] for a freshly downloaded
/// planet file at `pbf_path` -- its header (cheap, via [`read_header`])
/// and the SHA-256 of its entire contents (not cheap: one dedicated
/// sequential read pass over the whole file, see [`hash_file`]) -- and
/// persists it to `workdir`, so a later call in the same `workdir` reads
/// it back via [`read_cached_metadata`] instead of re-hashing.
pub(crate) fn compute_and_persist_metadata(
    pbf_path: &Path,
    workdir: &Path,
    progress: &MultiProgress,
) -> Result<OsmMetadata> {
    let header = read_header(pbf_path)?;
    let sha256 = hash_file(pbf_path, progress)?;
    let metadata = OsmMetadata {
        sha256: Some(sha256),
        ..header
    };
    let path = meta_json_path(workdir);
    let data = serde_json::to_string(&metadata)?;
    std::fs::write(&path, &data).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(metadata)
}

/// Reads `path` sequentially in fixed-size chunks and returns its
/// SHA-256 as lowercase hex, via the same `aws_lc_rs` crypto library
/// this crate already uses for TLS (see `build_client()` in
/// `main.rs`), not a second, separate hashing implementation.
///
/// Deliberately a plain buffered read, not `memmap2::Mmap` (which this
/// crate does use elsewhere, e.g. for the AllThePlaces zip): `path`
/// here is the OSM planet dump, tens of GB, and mmap'ing something that
/// large risks inflating RSS/page-cache accounting in ways that could
/// trip this pipeline's own cgroup memory-limit warnings (see
/// `pipeline::memstats`) for no benefit -- a small fixed buffer keeps
/// memory flat regardless of file size.
fn hash_file(path: &Path, progress: &MultiProgress) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("could not open file `{:?}`", path))?;
    let len = file.metadata().map(|m| m.len()).ok();
    let bar = make_download_bar(progress, "osm.hash      ", len);
    let mut hasher = DigestContext::new(&SHA256);
    let mut buf = vec![0u8; 8 * 1024 * 1024]; // 8 MiB
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("could not read file `{:?}`", path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        bar.inc(n as u64);
    }
    bar.finish();
    Ok(to_hex(hasher.finish().as_ref()))
}

/// Reads data blobs from OpenStreetMap PBF files.
struct BlobReader<'a, R: Read + Seek + Send> {
    reader: &'a mut R,

    header: OsmMetadata,

    /// Offset and size of each data blob.
    blobs: Vec<(u64, usize)>,
    node_blobs: Range<usize>,
    way_blobs: Range<usize>,
    relation_blobs: Range<usize>,
}

// SAFETY: Can be safely sent across threads, if the underlying reader
// implements the Send trait. With the type trait being declared as
// below (`+ Send`), this gets enforced by the Rust compiler.
unsafe impl<'a, R: Read + Seek + Send> Send for BlobReader<'a, R> {}

impl<'a, R: Read + Seek + Send> BlobReader<'a, R> {
    pub fn open(reader: &'a mut R) -> Result<BlobReader<'a, R>> {
        reader.seek(SeekFrom::End(0))?;
        let file_size = reader.stream_position()?;
        if file_size == 0 {
            return Err(anyhow!("empty file"));
        }
        let mut pos = 0_u64;
        let mut blobs = Vec::<(u64, usize)>::new();
        let mut header: Option<OsmMetadata> = None;
        while pos < file_size {
            reader.seek(SeekFrom::Start(pos))?;
            let blob_header = Self::read_blob_header(reader)?;
            let Some((blob_type, data_size)) = Self::parse_blob_header(&blob_header) else {
                return Err(anyhow!("bad blob header at offset {}", pos));
            };
            let offset = pos + 4_u64 + (blob_header.len() as u64);
            match blob_type {
                b"OSMHeader" => {
                    let blob = Self::read_blob(reader, offset, data_size)?;
                    header = Some(Self::parse_header_block(&blob.into_data())?);
                }
                b"OSMData" => {
                    blobs.push((offset, data_size));
                }
                _ => {}
            }
            pos += 4_u64 + (blob_header.len() as u64) + (data_size as u64);
        }
        let header = header.ok_or_else(|| anyhow!("PBF file has no OSMHeader block"))?;

        let (nodes_end, ways_end) = Self::partition(reader, &blobs)?;
        let relations_end = blobs.len();
        Ok(BlobReader {
            reader,
            header,
            blobs,
            node_blobs: 0..nodes_end,
            way_blobs: nodes_end.saturating_sub(1)..ways_end,
            relation_blobs: ways_end.saturating_sub(1)..relations_end,
        })
    }

    pub fn header(&self) -> &OsmMetadata {
        &self.header
    }

    pub fn count_node_blobs(&self) -> usize {
        self.node_blobs.len()
    }

    pub fn count_way_blobs(&self) -> usize {
        self.way_blobs.len()
    }

    pub fn count_relation_blobs(&self) -> usize {
        self.relation_blobs.len()
    }

    pub fn send_node_blobs(&mut self, tx: SyncSender<Blob>) -> Result<()> {
        for i in self.node_blobs.clone() {
            let (offset, len) = self.blobs[i];
            tx.send(Self::read_blob(self.reader, offset, len)?)?;
        }
        Ok(())
    }

    pub fn send_way_blobs(&mut self, tx: SyncSender<Blob>) -> Result<()> {
        for i in self.way_blobs.clone() {
            let (offset, len) = self.blobs[i];
            tx.send(Self::read_blob(self.reader, offset, len)?)?;
        }
        Ok(())
    }

    pub fn send_relation_blobs(&mut self, tx: SyncSender<Blob>) -> Result<()> {
        for i in self.relation_blobs.clone() {
            let (offset, len) = self.blobs[i];
            tx.send(Self::read_blob(self.reader, offset, len)?)?;
        }
        Ok(())
    }

    fn read_blob(reader: &mut R, offset: u64, len: usize) -> Result<Blob> {
        let mut buf = Vec::with_capacity(len);
        reader.seek(SeekFrom::Start(offset))?;

        // SAFETY: After read_exact(), all bytes in buffer have a defined value.
        unsafe {
            buf.set_len(len);
            reader.read_exact(&mut buf)?;
        }
        Self::decode_blob(&buf)
    }

    fn decode_blob(data: &[u8]) -> Result<Blob> {
        for m in MessageIter::new(data) {
            match m.tag {
                1 => return Ok(Blob::Raw(Vec::from(m.value.get_data()))),
                3 => return Ok(Blob::Zlib(Vec::from(m.value.get_data()))),
                _ => {}
            }
        }

        Err(anyhow!("cannot decode blob"))
    }

    /// Partitions the blogs into nodes, ways and relations.
    ///
    /// # Returns
    ///
    /// A tuple `(a, b)` where `a` is the first blob without any nodes,
    /// and `b` is the first blob without either nodes or ways.
    ///
    /// # Warnings
    ///
    /// In the
    /// [OpenStreetMap PBF format](https://wiki.openstreetmap.org/wiki/PBF_Format),
    /// a single blog may contain repeated PrimitiveGroups. While all primitives
    /// in the same must be of the same type (node, way or relation), the format
    /// makes no such guarantee on the blob level.
    fn partition(reader: &mut R, blobs: &[(u64, usize)]) -> Result<(usize, usize)> {
        let ways = {
            let mut left = 0;
            let mut right = blobs.len();
            while left < right {
                let mid = left + (right - left) / 2;
                let blob = Self::read_blob(reader, blobs[mid].0, blobs[mid].1)?;
                if Self::classify(blob)? < 2 {
                    left = mid + 1;
                } else {
                    right = mid;
                }
            }
            left
        };

        let relations = {
            let mut left = ways;
            let mut right = blobs.len();
            while left < right {
                let mid = left + (right - left) / 2;
                let blob = Self::read_blob(reader, blobs[mid].0, blobs[mid].1)?;
                if Self::classify(blob)? < 3 {
                    left = mid + 1;
                } else {
                    right = mid;
                }
            }
            left
        };

        Ok((ways, relations))
    }

    /// Internal helper for partition().
    fn classify(blob: Blob) -> Result<u8> {
        let data = blob.into_data();
        let block = PrimitiveBlock::parse(&data);
        match block.primitives().next() {
            Some(Primitive::Node(_)) => Ok(1),
            Some(Primitive::Way(_)) => Ok(2),
            Some(Primitive::Relation(_)) => Ok(3),
            None => Err(anyhow!("empty blob")),
        }
    }

    fn read_blob_header<T: Read>(reader: &mut T) -> Result<Vec<u8>> {
        let header_len = {
            let mut header_len_buf = [0; 4];
            reader.read_exact(&mut header_len_buf)?;
            u32::from_be_bytes(header_len_buf) as usize
        };
        let mut header = vec![0; header_len];
        reader.read_exact(&mut header)?;
        Ok(header)
    }

    fn parse_blob_header(data: &[u8]) -> Option<(&[u8], usize)> {
        let mut blob_type: Option<&[u8]> = None;
        let mut data_size: Option<usize> = None;
        for m in MessageIter::new(data) {
            match m.tag {
                1 => blob_type = Some(m.value.get_data()),
                3 => data_size = Some(u32::from(m.value) as usize),
                _ => {}
            }
        }
        Some((blob_type?, data_size?))
    }

    /// Parses the fields we care about out of a decoded `OSMHeader` blob,
    /// i.e. the `HeaderBlock` message from OSM's `osmformat.proto`.
    /// `osm_pbf_iter`/`protobuf_iter` don't support this message, so we
    /// pick out the fields we need by hand:
    ///
    /// - `writingprogram` (tag 16, string)
    /// - `source` (tag 17, string)
    /// - `osmosis_replication_timestamp` (tag 32, int64)
    ///
    /// Deliberately not parsed: `osmosis_replication_sequence_number`
    /// (tag 33, int64). `planet-dump-ng` -- which produces the actual
    /// planet dumps we process (confirmed as of this writing, on the
    /// dump timestamped 2026-07-27T15:16:41Z) -- never writes it; this
    /// is a known, still-open upstream gap, blocked on figuring out
    /// which `state.txt` sequence number a given dump corresponds to:
    /// <https://github.com/zerebubuth/planet-dump-ng/issues/16>, blocked
    /// on <https://github.com/zerebubuth/planet-dump-ng/issues/6>.
    fn parse_header_block(data: &[u8]) -> Result<OsmMetadata> {
        const WRITING_PROGRAM: u32 = 16;
        const SOURCE: u32 = 17;
        const OSMOSIS_REPLICATION_TIMESTAMP: u32 = 32;

        let mut replication_timestamp = None;
        let mut source = None;
        let mut writing_program = None;
        for m in MessageIter::new(data) {
            match m.tag {
                WRITING_PROGRAM => {
                    writing_program =
                        Some(String::from_utf8_lossy(m.value.get_data()).into_owned());
                }
                SOURCE => {
                    source = Some(String::from_utf8_lossy(m.value.get_data()).into_owned());
                }
                OSMOSIS_REPLICATION_TIMESTAMP => {
                    // NB: `osmosis_replication_timestamp` is a plain `int64`,
                    // not `sint64`, so it is *not* zigzag-encoded on the
                    // wire. Go through `u64::from` (which returns the raw
                    // varint) rather than protobuf_iter's `i64::from`, which
                    // always zigzag-decodes and would silently corrupt the
                    // value here.
                    let secs = u64::from(m.value) as i64;
                    replication_timestamp = Some(UtcDateTime::from_unix_timestamp(secs)?);
                }
                _ => {}
            }
        }

        Ok(OsmMetadata {
            replication_timestamp: replication_timestamp
                .ok_or_else(|| anyhow!("OSMHeader block has no osmosis_replication_timestamp"))?,
            source,
            writing_program,
            sha256: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::PathBuf;

    #[test]
    fn test_encode_decode_feature_id_round_trip() {
        for (member_type, id) in [
            (RelationMemberType::Node, 123),
            (RelationMemberType::Way, 608979139),
            (RelationMemberType::Relation, 999_999_999),
        ] {
            let fid = encode_feature_id(member_type.clone(), id);
            assert_eq!(decode_feature_id(fid), Some((member_type, id)));
        }
    }

    #[test]
    fn test_decode_feature_id_rejects_bad_last_digit() {
        assert_eq!(decode_feature_id(1230), None);
        assert_eq!(decode_feature_id(1234), None);
    }

    #[test]
    fn test_osm_type_str() {
        assert_eq!(osm_type_str(RelationMemberType::Node), "node");
        assert_eq!(osm_type_str(RelationMemberType::Way), "way");
        assert_eq!(osm_type_str(RelationMemberType::Relation), "relation");
    }
    use std::sync::mpsc::sync_channel;

    #[test]
    fn test_blob_reader() -> Result<()> {
        let mut file = File::open(test_data_path("zugerland.osm.pbf"))?;
        let mut reader = BlobReader::open(&mut file)?;
        assert_eq!(reader.blobs, &[(119, 16681), (16816, 15278), (32110, 8616)]);
        assert_eq!(
            reader.header().replication_timestamp,
            UtcDateTime::from_unix_timestamp(1769501462)? // 2026-01-27T08:11:02Z
        );
        assert_eq!(reader.header().source, None);
        assert_eq!(reader.header().writing_program, Some("osmx".to_owned()));
        assert_eq!(reader.node_blobs, 0..1);
        assert_eq!(reader.way_blobs, 0..2);
        assert_eq!(reader.relation_blobs, 1..3);
        let (tx, rx) = sync_channel::<Blob>(5);
        reader.send_node_blobs(tx)?;
        if let Blob::Zlib(_) = rx.recv()? {
        } else {
            return Err(anyhow!("failed to read blob"));
        }
        Ok(())
    }

    #[test]
    fn test_read_header() -> Result<()> {
        // Must agree with what BlobReader::open() -- which scans the
        // whole file -- reports for the same fixture.
        let metadata = read_header(&test_data_path("zugerland.osm.pbf"))?;
        assert_eq!(
            metadata.replication_timestamp,
            UtcDateTime::from_unix_timestamp(1769501462)? // 2026-01-27T08:11:02Z
        );
        assert_eq!(metadata.source, None);
        assert_eq!(metadata.writing_program, Some("osmx".to_owned()));
        Ok(())
    }

    #[test]
    fn test_read_header_rejects_bad_blob_header() {
        use std::io::Write;

        // A zero-length blob header (same bad input as
        // test_blob_reader_bad_data's b"\0\0\0\0" case): read_blob_header
        // succeeds trivially, but parse_blob_header then finds neither a
        // blob type nor a data size in the (empty) header.
        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        file.write_all(b"\0\0\0\0").expect("write");
        let err = read_header(file.path()).unwrap_err();
        assert!(err.to_string().contains("bad blob header"));
    }

    #[test]
    fn test_read_header_rejects_wrong_first_blob_type() {
        use std::io::Write;

        // A well-formed blob header, but for an "OSMData" blob rather
        // than "OSMHeader" -- as if handed a PBF file's data blob
        // directly, without the header block that should precede it.
        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        let mut blob_header = Vec::new();
        // Tag 1 (blob_type, string): "OSMData".
        blob_header.extend_from_slice(&[0x0a, 7]);
        blob_header.extend_from_slice(b"OSMData");
        // Tag 3 (datasize, varint): 0.
        blob_header.extend_from_slice(&[0x18, 0]);
        file.write_all(&(blob_header.len() as u32).to_be_bytes())
            .expect("write");
        file.write_all(&blob_header).expect("write");
        let err = read_header(file.path()).unwrap_err();
        assert!(err.to_string().contains("OSMData"));
    }

    #[test]
    fn test_read_header_missing_file() {
        assert!(read_header(Path::new("/no/such/file.osm.pbf")).is_err());
    }

    #[test]
    fn test_blob_reader_decode() -> Result<()> {
        if let Blob::Raw(blob) = BlobReader::<File>::decode_blob(&[0x0a, 1, 77])? {
            assert_eq!(blob, &[77]);
        } else {
            panic!("unexpected blob type");
        }
        if let Blob::Zlib(blob) = BlobReader::<File>::decode_blob(&[0x1a, 1, 77])? {
            assert_eq!(blob, &[77]);
        } else {
            panic!("unexpected blob type");
        }
        assert!(BlobReader::<File>::decode_blob(&[0x2a, 1, 77]).is_err());
        Ok(())
    }

    #[test]
    fn test_blob_reader_bad_data() {
        assert!(BlobReader::open(&mut Cursor::new(b"")).is_err());
        assert!(BlobReader::open(&mut Cursor::new(b"\0\0\0")).is_err());
        assert!(BlobReader::open(&mut Cursor::new(b"\0\0\0\0")).is_err());
        assert!(BlobReader::open(&mut Cursor::new(b"test file with junk data")).is_err());
    }

    fn test_data_path(filename: &str) -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests");
        path.push("test_data");
        path.push(filename);
        path
    }
}
