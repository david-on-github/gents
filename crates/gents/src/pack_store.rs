//! The content-addressed pack store: `{home}/packs/store/sha256/{hex}.pack`.
//!
//! A file lands in the store only after [`read_pack`] verified it, streaming,
//! while its bytes were copied into a staging file beside the destination.
//! The staging file is renamed into place only when the computed digest
//! equals both the file's own header and the digest the caller asked for, so
//! a reader never sees a partial or unverified pack under a digest's name.
//! Because a name is its content, an existing file is never replaced.
//!
//! Beside the digest-addressed store sits a name index,
//! `{home}/packs/store/by-name/{namespace}/{name}/{version}`, a file
//! holding that version's digest. [`Self::import_accepting`] writes it for
//! every import (registry fetch, `.pack` file or directory alike), so
//! [`Self::lookup`] can resolve a bare `namespace/name[@version]` to a
//! stored archive without opening any of them.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};

use crate::pack::is_valid_pack_name;
use crate::pack_archive::{
    digest_hex, peek_header, read_pack, Bounds, PackArchive, PackHeader, EXTENSION,
};

/// The name index directory, sibling of `sha256/` under the store root.
const NAME_INDEX_DIR_NAME: &str = "by-name";

/// The verified header an unpacked pack keeps beside its files; a dotfile,
/// so it can never be mistaken for a pack asset.
const UNPACKED_HEADER: &str = ".pack-header.json";

/// A home's pack store.
#[derive(Debug, Clone)]
pub struct PackStore {
    root: PathBuf,
}

/// A pack the store holds.
#[derive(Debug, Clone)]
pub struct StoredPack {
    pub header: PackHeader,
    pub path: PathBuf,
}

/// One version the name index has recorded for a `namespace/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredName {
    pub version: String,
    pub digest: String,
}

/// A version string's sort key: a valid semantic version orders by
/// [`semver::Version`]; anything else orders after every semantic version,
/// lexicographically among themselves, so a real release always outranks
/// an oddball tag like `latest`. Declaring `Other` first is load bearing:
/// the derived `Ord` compares the variant before its payload, so every
/// `Other` sorts below every `Semver` regardless of their text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum VersionKey {
    Other(String),
    Semver(semver::Version),
}

impl VersionKey {
    fn parse(version: &str) -> Self {
        semver::Version::parse(version)
            .map(VersionKey::Semver)
            .unwrap_or_else(|_| VersionKey::Other(version.to_owned()))
    }
}

/// Whether `version` is safe as one path segment of the name index: ASCII
/// lowercase alphanumeric plus `.`, `+`, `-`, non-empty, and never a bare `.`
/// or `..` (otherwise a valid-looking version could name the parent
/// directory). Uppercase is refused, not folded: the index directory can sit
/// on a case-insensitive filesystem (macOS APFS by default), where
/// `1.0.0-RC1` and `1.0.0-rc1` would otherwise silently collide and one
/// would overwrite the other's entry. `pub(crate)` so [`crate::pack_resolve`]
/// holds a pinned `@version` to the same rule the index files it against.
pub(crate) fn is_valid_index_version(version: &str) -> bool {
    version != "."
        && version != ".."
        && !version.is_empty()
        && version.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'+' | b'-')
        })
}

impl PackStore {
    /// The store of the gents home at `home`.
    pub fn new(home: &Path) -> Self {
        Self {
            root: home
                .join(crate::home::PACKS_DIR_NAME)
                .join("store")
                .join("sha256"),
        }
    }

    /// Where the pack with `digest` lives, whether or not it is there.
    pub fn path(&self, digest: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join(format!("{}.{EXTENSION}", digest_hex(digest)?)))
    }

    pub fn contains(&self, digest: &str) -> Result<bool> {
        Ok(self.path(digest)?.is_file())
    }

    /// Verifies `input` as a `.pack` and stores it under its digest. With
    /// `expected`, a pack with any other digest is refused and nothing is
    /// stored.
    pub fn import(&self, input: impl Read, expected: Option<&str>) -> Result<StoredPack> {
        self.import_accepting(input, expected, |_| Ok(()))
    }

    /// The same, refusing the pack when `accept` does, before anything is
    /// stored under its digest.
    pub fn import_accepting(
        &self,
        input: impl Read,
        expected: Option<&str>,
        accept: impl FnOnce(&crate::pack_archive::VerifiedPack) -> Result<()>,
    ) -> Result<StoredPack> {
        if let Some(expected) = expected {
            digest_hex(expected)?;
        }
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating the pack store {}", self.root.display()))?;
        let mut staged = tempfile::Builder::new()
            .prefix(".staging-")
            .tempfile_in(&self.root)
            .with_context(|| format!("staging a pack in {}", self.root.display()))?;
        let verified = {
            // `read_pack` reads the input to its end, so the copy is the whole file.
            let mut tee = TeeReader {
                inner: input,
                copy: io::BufWriter::new(staged.as_file_mut()),
            };
            let verified = read_pack(&mut tee, Bounds::default(), |_, _| Ok(()))?;
            tee.copy.flush().context("writing the staged pack")?;
            verified
        };
        accept(&verified)?;
        let digest = &verified.header.digest;
        if let Some(expected) = expected {
            ensure!(
                digest == expected,
                "asked for pack {expected} but received {digest}; nothing was stored"
            );
        }
        staged
            .as_file()
            .sync_all()
            .context("syncing the staged pack")?;
        let path = self.path(digest)?;
        match staged.persist_noclobber(&path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error.error)
                    .with_context(|| format!("storing the pack at {}", path.display()))
            }
        }
        self.index(&verified.header)?;
        Ok(StoredPack {
            header: verified.header,
            path,
        })
    }

    /// Stores the `.pack` file at `file`.
    pub fn import_file(&self, file: &Path, expected: Option<&str>) -> Result<StoredPack> {
        self.import_file_accepting(file, expected, |_| Ok(()))
    }

    /// [`Self::import_accepting`] for a file.
    pub fn import_file_accepting(
        &self,
        file: &Path,
        expected: Option<&str>,
        accept: impl FnOnce(&crate::pack_archive::VerifiedPack) -> Result<()>,
    ) -> Result<StoredPack> {
        let input =
            std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
        self.import_accepting(io::BufReader::new(input), expected, accept)
            .with_context(|| format!("{} is not a valid pack", file.display()))
    }

    /// Opens the stored pack `digest`. The first open unpacks it, verified
    /// while streaming, into `{home}/packs/unpacked/{hex}/` (staged, then
    /// renamed into place); every open maps its files from there, so a pack
    /// of any size costs page cache, not memory. [`Self::verify`] checks the
    /// stored file again from scratch.
    pub fn open(&self, digest: &str) -> Result<PackArchive> {
        let hex = digest_hex(digest)?;
        let unpacked = self.unpacked_root().join(hex);
        if !unpacked.is_dir() {
            self.unpack(digest, &unpacked)?;
        }
        let header: PackHeader = serde_json::from_slice(
            &std::fs::read(unpacked.join(UNPACKED_HEADER))
                .with_context(|| format!("reading {}", unpacked.display()))?,
        )
        .context("the unpacked pack's header is not valid")?;
        ensure!(
            header.digest == digest,
            "the unpacked pack at {} holds {}, not {digest}",
            unpacked.display(),
            header.digest
        );
        let manifest = serde_json::from_slice(
            &std::fs::read(unpacked.join("manifest.json")).context("reading manifest.json")?,
        )
        .context("the unpacked manifest is not valid")?;
        PackArchive::from_unpacked(&unpacked, header, manifest)
    }

    fn unpacked_root(&self) -> PathBuf {
        self.root.parent().and_then(Path::parent).map_or_else(
            || self.root.join("unpacked"),
            |packs| packs.join("unpacked"),
        )
    }

    /// `{home}/packs/store/by-name`, the name index root.
    fn by_name_root(&self) -> PathBuf {
        self.root.parent().map_or_else(
            || self.root.join(NAME_INDEX_DIR_NAME),
            |store| store.join(NAME_INDEX_DIR_NAME),
        )
    }

    /// Records `header` in the name index, replacing any prior entry for the
    /// same namespace/name/version. Called from [`Self::import_accepting`],
    /// after the archive itself is persisted, and from
    /// [`crate::pack_registry::fetch_pack`]'s by-download cache hit, which
    /// opens an already-stored archive without going through
    /// `import_accepting` again: either way, this is the one place a header
    /// becomes a name-index entry.
    ///
    /// A coordinate or version this store cannot use as a path segment is
    /// logged and left out of the index: the archive is still stored under
    /// its digest either way, so only the by-name lookup misses it.
    pub(crate) fn index(&self, header: &PackHeader) -> Result<()> {
        let Some((namespace, name)) = header.coordinate.split_once('/') else {
            tracing::warn!(
                coordinate = %header.coordinate,
                "pack coordinate is not namespace/name; not indexing it by name"
            );
            return Ok(());
        };
        if !is_valid_pack_name(namespace) || !is_valid_pack_name(name) {
            tracing::warn!(
                coordinate = %header.coordinate,
                "pack coordinate is not snake_case; not indexing it by name"
            );
            return Ok(());
        }
        if !is_valid_index_version(&header.version) {
            tracing::warn!(
                coordinate = %header.coordinate,
                version = %header.version,
                "pack version cannot be indexed by name"
            );
            return Ok(());
        }
        let dir = self.by_name_root().join(namespace).join(name);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating the pack name index at {}", dir.display()))?;
        let mut staged = tempfile::Builder::new()
            .prefix(".staging-")
            .tempfile_in(&dir)
            .with_context(|| format!("staging a pack name index entry in {}", dir.display()))?;
        staged
            .write_all(header.digest.as_bytes())
            .context("writing a pack name index entry")?;
        staged
            .as_file()
            .sync_all()
            .context("syncing a pack name index entry")?;
        let target = dir.join(&header.version);
        staged
            .persist(&target)
            .map_err(|error| error.error)
            .with_context(|| {
                format!("storing the pack name index entry at {}", target.display())
            })?;
        Ok(())
    }

    /// The versions indexed under `dir` (one `namespace/name` directory)
    /// whose archive is still present in the store, in arbitrary order. A
    /// stale entry (raced with [`Self::release`]) or a corrupt one (a
    /// malformed digest) is logged and skipped rather than failing the
    /// whole lookup.
    fn read_indexed_versions(&self, dir: &Path) -> Result<Vec<StoredName>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
        };
        let mut versions = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let Some(version) = entry.file_name().to_str().map(str::to_owned) else {
                continue; // never written by this store
            };
            if version.starts_with(".staging-") {
                continue; // an in-flight write
            }
            let digest = match std::fs::read_to_string(entry.path()) {
                Ok(digest) => digest.trim().to_owned(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue, // raced with a release
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reading {}", entry.path().display()))
                }
            };
            let present = match self.contains(&digest) {
                Ok(present) => present,
                Err(error) => {
                    tracing::warn!(
                        entry = %entry.path().display(),
                        %error,
                        "pack name index entry has a malformed digest; skipping it"
                    );
                    false
                }
            };
            if present {
                versions.push(StoredName { version, digest });
            }
        }
        Ok(versions)
    }

    /// The store's resolution for `namespace/name`: `version` exactly when
    /// it was indexed and its archive is still present, otherwise the
    /// highest semantic version among indexed entries whose archive is
    /// present (non-semver versions sort after every semantic one,
    /// lexicographically).
    pub fn lookup(
        &self,
        namespace: &str,
        name: &str,
        version: Option<&str>,
    ) -> Result<Option<StoredName>> {
        let dir = self.by_name_root().join(namespace).join(name);
        if let Some(version) = version {
            return self.lookup_exact_version(&dir, version);
        }
        let mut versions = self.read_indexed_versions(&dir)?;
        versions.sort_by(|a, b| VersionKey::parse(&a.version).cmp(&VersionKey::parse(&b.version)));
        Ok(versions.pop())
    }

    /// [`Self::lookup`] for one named version: the version is already the
    /// index file's name, so this reads that one file directly instead of
    /// listing and reading every version under `dir`.
    fn lookup_exact_version(&self, dir: &Path, version: &str) -> Result<Option<StoredName>> {
        if !is_valid_index_version(version) {
            // `Self::index` never files a version under this shape, so an
            // exact lookup for it can never hit.
            return Ok(None);
        }
        let path = dir.join(version);
        let digest = match std::fs::read_to_string(&path) {
            Ok(digest) => digest.trim().to_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let present = match self.contains(&digest) {
            Ok(present) => present,
            Err(error) => {
                tracing::warn!(
                    entry = %path.display(),
                    %error,
                    "pack name index entry has a malformed digest; skipping it"
                );
                false
            }
        };
        Ok(present.then(|| StoredName {
            version: version.to_owned(),
            digest,
        }))
    }

    /// Every `namespace/name` the name index has an entry for, each with its
    /// indexed versions whose archive is still present, newest first
    /// (non-semver versions last, lexicographically), sorted by coordinate.
    pub fn names(&self) -> Result<Vec<(String, Vec<StoredName>)>> {
        let root = self.by_name_root();
        let namespace_entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", root.display())),
        };
        let mut names = Vec::new();
        for namespace_entry in namespace_entries {
            let namespace_entry =
                namespace_entry.with_context(|| format!("reading {}", root.display()))?;
            if !namespace_entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(namespace) = namespace_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let namespace_dir = namespace_entry.path();
            let name_entries = std::fs::read_dir(&namespace_dir)
                .with_context(|| format!("reading {}", namespace_dir.display()))?;
            for name_entry in name_entries {
                let name_entry =
                    name_entry.with_context(|| format!("reading {}", namespace_dir.display()))?;
                if !name_entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let Some(name) = name_entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let mut versions = self.read_indexed_versions(&name_entry.path())?;
                if versions.is_empty() {
                    continue;
                }
                versions.sort_by(|a, b| {
                    VersionKey::parse(&b.version).cmp(&VersionKey::parse(&a.version))
                });
                names.push((format!("{namespace}/{name}"), versions));
            }
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(names)
    }

    /// The `namespace, name, version` the digest about to be released was
    /// filed under, read cheaply from what release is about to delete rather
    /// than by scanning the whole index: the unpacked header if this digest
    /// was ever opened, otherwise the stored archive's own first entry
    /// ([`peek_header`], which reads only that far). `None` when neither is
    /// present, meaning the digest is already gone from the content-addressed
    /// store (a prior release, or a name-index entry left over from before
    /// this pack was ever stored under it); [`Self::release`] falls back to
    /// [`Self::remove_name_entries_for_digest`] in that case.
    ///
    /// One header names exactly one entry: `manifest.json`, which carries
    /// the namespace, name and version, is itself part of what the pack
    /// digest is computed over (see [`crate::pack_archive::read_pack`]), so
    /// two different coordinates or versions can never share a digest.
    fn indexed_coordinate_and_version(
        &self,
        archive: &Path,
        unpacked: &Path,
    ) -> Result<Option<(String, String, String)>> {
        let header = if let Ok(bytes) = std::fs::read(unpacked.join(UNPACKED_HEADER)) {
            serde_json::from_slice(&bytes).context("the unpacked pack's header is not valid")?
        } else {
            match std::fs::File::open(archive) {
                Ok(file) => peek_header(io::BufReader::new(file), Bounds::default())
                    .with_context(|| format!("reading the header of {}", archive.display()))?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error).with_context(|| format!("opening {}", archive.display()))
                }
            }
        };
        let Some((namespace, name)) = header.coordinate.split_once('/') else {
            return Ok(None);
        };
        Ok(Some((
            namespace.to_owned(),
            name.to_owned(),
            header.version,
        )))
    }

    /// Removes `by-name/{namespace}/{name}/{version}`, only if it currently
    /// records `digest`: the one entry [`Self::indexed_coordinate_and_version`]
    /// can name directly.
    fn remove_indexed_entry(
        &self,
        namespace: &str,
        name: &str,
        version: &str,
        digest: &str,
    ) -> Result<()> {
        let path = self.by_name_root().join(namespace).join(name).join(version);
        let recorded = match std::fs::read_to_string(&path) {
            Ok(recorded) => recorded,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        if recorded.trim() != digest {
            return Ok(());
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// Fallback for [`Self::release`] when the digest's header can no longer
    /// be read (the archive and its unpacked copy are both already gone):
    /// scans every name-index entry and deletes the ones recording `digest`.
    /// Cost scales with the whole index rather than with the one pack being
    /// released, so [`Self::release`] only reaches for this when it has no
    /// cheaper way to find the entry.
    fn remove_name_entries_for_digest(&self, digest: &str) -> Result<()> {
        let root = self.by_name_root();
        let namespace_entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", root.display())),
        };
        for namespace_entry in namespace_entries {
            let namespace_dir = namespace_entry
                .with_context(|| format!("reading {}", root.display()))?
                .path();
            if !namespace_dir.is_dir() {
                continue;
            }
            for name_entry in std::fs::read_dir(&namespace_dir)
                .with_context(|| format!("reading {}", namespace_dir.display()))?
            {
                let name_dir = name_entry
                    .with_context(|| format!("reading {}", namespace_dir.display()))?
                    .path();
                if !name_dir.is_dir() {
                    continue;
                }
                for version_entry in std::fs::read_dir(&name_dir)
                    .with_context(|| format!("reading {}", name_dir.display()))?
                {
                    let path = version_entry
                        .with_context(|| format!("reading {}", name_dir.display()))?
                        .path();
                    let recorded = match std::fs::read_to_string(&path) {
                        Ok(recorded) => recorded,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("reading {}", path.display()))
                        }
                    };
                    if recorded.trim() != digest {
                        continue;
                    }
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("removing {}", path.display()))
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn unpack(&self, digest: &str, target: &Path) -> Result<()> {
        let path = self.path(digest)?;
        let file = std::fs::File::open(&path).with_context(|| {
            format!(
                "pack {digest} is not in the store at {}",
                self.root.display()
            )
        })?;
        let parent = target.parent().context("unpacked path has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let staging = tempfile::Builder::new()
            .prefix(".unpacking-")
            .tempdir_in(parent)
            .with_context(|| format!("staging an unpacked pack in {}", parent.display()))?;
        let verified = read_pack(
            io::BufReader::new(file),
            Bounds::default(),
            |entry, content| {
                let out = staging.path().join(entry);
                if let Some(dir) = out.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                io::copy(
                    content,
                    &mut io::BufWriter::new(std::fs::File::create(&out)?),
                )?;
                Ok(())
            },
        )
        .with_context(|| format!("the stored pack {} is damaged", path.display()))?;
        ensure!(
            verified.header.digest == digest,
            "the stored pack {} holds {}, not {digest}",
            path.display(),
            verified.header.digest
        );
        std::fs::write(
            staging.path().join("manifest.json"),
            &verified.manifest_bytes,
        )
        .context("writing the unpacked manifest")?;
        std::fs::write(
            staging.path().join(UNPACKED_HEADER),
            serde_json::to_vec(&verified.header)?,
        )
        .context("writing the unpacked header")?;
        match std::fs::rename(staging.path(), target) {
            Ok(()) => {
                // The directory now lives at its final name.
                let _ = staging.keep();
                Ok(())
            }
            // Another open unpacked the same digest first; its copy is identical.
            Err(_) if target.is_dir() => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("unpacking into {}", target.display()))
            }
        }
    }

    /// Removes the stored archive for `digest` and its unpacked copy, if
    /// either is present. Idempotent: releasing a digest already gone, or
    /// never stored, is not an error. Returns whether anything was removed.
    pub fn release(&self, digest: &str) -> Result<bool> {
        let hex = digest_hex(digest)?;
        let archive = self.path(digest)?;
        let unpacked = self.unpacked_root().join(hex);
        // Read before deleting: once the archive and its unpacked copy are
        // both gone, there is no cheap way left to name the one index entry
        // this digest was filed under.
        let coordinate_and_version = self.indexed_coordinate_and_version(&archive, &unpacked)?;
        let mut released = false;
        match std::fs::remove_file(&archive) {
            Ok(()) => released = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("removing {}", archive.display()))
            }
        }
        match std::fs::remove_dir_all(&unpacked) {
            Ok(()) => released = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("removing {}", unpacked.display()))
            }
        }
        match coordinate_and_version {
            Some((namespace, name, version)) => {
                self.remove_indexed_entry(&namespace, &name, &version, digest)?
            }
            None => self.remove_name_entries_for_digest(digest)?,
        }
        Ok(released)
    }

    /// Verifies the stored pack `digest` without holding its assets.
    pub fn verify(&self, digest: &str) -> Result<PackHeader> {
        let path = self.path(digest)?;
        let file = std::fs::File::open(&path).with_context(|| {
            format!(
                "pack {digest} is not in the store at {}",
                self.root.display()
            )
        })?;
        let verified = read_pack(io::BufReader::new(file), Bounds::default(), |_, _| Ok(()))
            .with_context(|| format!("the stored pack {} is damaged", path.display()))?;
        ensure!(
            verified.header.digest == digest,
            "the stored pack {} holds {}, not {digest}",
            path.display(),
            verified.header.digest
        );
        Ok(verified.header)
    }
}

/// Copies every byte read from `inner` into `copy`.
struct TeeReader<R, W> {
    inner: R,
    copy: W,
}

impl<R: Read, W: Write> Read for TeeReader<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.copy.write_all(&buf[..n])?;
        Ok(n)
    }
}

/// The `assets_fixture` fixture pack, renamed and re-versioned, for tests
/// that need a pack under a coordinate and version of their own choosing
/// rather than its real one. Shared by this module's tests and
/// [`crate::pack_resolve`]'s, so the two do not carry copies of the same
/// fixture-building code.
#[cfg(test)]
pub(crate) fn test_pack_named(name: &str, version: &str) -> (Vec<u8>, PackHeader) {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/packs/assets_fixture");
    let manifest: crate::pack::PackManifest =
        serde_json::from_slice(&std::fs::read(source.join("manifest.json")).unwrap()).unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    for path in crate::pack::declared_paths(&manifest) {
        let target = dir.path().join(&path);
        std::fs::create_dir_all(target.parent().expect("a parent")).expect("mkdir");
        std::fs::copy(source.join(&path), &target).expect("copy");
    }
    let manifest_path = dir.path().join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["namespace"] = serde_json::json!("gents");
    manifest["name"] = serde_json::json!(name);
    manifest["version"] = serde_json::json!(version);
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    crate::pack_archive::pack_dir(dir.path()).expect("packing")
}

#[cfg(test)]
mod tests;
