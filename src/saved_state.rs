//! Saved states: the state of a running wasm program in a file, from which
//! the program can be resumed later, also by another m-browser process.
//!
//! This module is what every kind of program shares: the file's name, its
//! header (which program, which module, when, for which tab) and its layout.
//! What goes into the sections is up to the engine that saved it:
//!
//! - `snapshot`: the generic engine for core modules. It saves the wasm
//!   state (memories, mutable globals, table sizes) at a *save point*, a
//!   moment when none of the program's functions is running.
//! - `wasi`: the WASI preview 1 part of the host state: open files and the
//!   monotonic clock.
//!
//! File layout, all numbers little-endian:
//!
//! ```text
//! "WASMSAVE" | format version u32 | header length u32 | header | CRC32 u32
//! section*:  tag [u8; 4] | payload length u64 | payload | CRC32 u32
//! a section tagged "END\0" with an empty payload
//! ```
//!
//! The header and most payloads are postcard-encoded serde structs; memory
//! images are sequences of deflate-compressed 64 KiB chunks (see
//! `snapshot`). The CRC32 of each part detects a damaged file before any of
//! it is used.

pub mod snapshot;
pub mod wasi;

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// First bytes of every saved-state file.
pub const MAGIC: &[u8; 8] = b"WASMSAVE";
/// Version of the file layout. Files with a newer version are refused.
pub const FORMAT_VERSION: u32 = 1;
/// File name extension of saved states.
pub const EXTENSION: &str = "wasmsavestate";

pub const TAG_WASI: [u8; 4] = *b"WASI";
pub const TAG_ASYNCIFY: [u8; 4] = *b"ASYN";
pub const TAG_GLOBALS: [u8; 4] = *b"GLOB";
pub const TAG_TABLES: [u8; 4] = *b"TABL";
pub const TAG_MEMORY: [u8; 4] = *b"MEMO";
pub const TAG_QEMU: [u8; 4] = *b"QEMU";
pub const TAG_END: [u8; 4] = *b"END\0";

/// Which engine saved a state, and so which one can resume it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// `snapshot`: the wasm state of a core module, plus its host state.
    WasmSnapshot,
    /// `qemu_save`: QEMU's own migration stream, written into the file
    /// after the manifest, and the frozen disk images it goes with.
    QemuMigration,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The program stopped when it was saved and continues from the file.
    Hibernate,
    /// The program kept running after the save.
    Checkpoint,
}

/// The module a state was saved from. A state resumes only with a module
/// whose bytes have the same SHA-256.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ModuleId {
    /// Absolute host path the module was loaded from.
    pub path: String,
    pub sha256: [u8; 32],
    pub size: u64,
}

impl ModuleId {
    /// Read a module file, and identify it.
    pub fn of_file(path: &Path) -> Result<(Self, Vec<u8>)> {
        use sha2::Digest;
        let wasm = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        let id = ModuleId {
            path: path.to_string_lossy().into_owned(),
            sha256: sha2::Sha256::digest(&wasm).into(),
            size: wasm.len() as u64,
        };
        Ok((id, wasm))
    }

    /// The hash in hex, as `sha256sum` prints it.
    pub fn sha256_hex(&self) -> String {
        self.sha256.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// How a program was started, so that it can be started the same way again.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// A core module run by `--wasm-headless` (wasm_headless.rs).
    Core(CoreLaunch),
    /// QEMU, started by `wasm::run_wasm_module` (qemu_save.rs).
    Qemu(QemuLaunch),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct QemuLaunch {
    /// QEMU's arguments, argv[0] first, with each disk's `-drive file=`
    /// naming the image frozen with the state.
    pub args: Vec<String>,
    /// Preopened directories: (absolute host path, path QEMU sees).
    pub preopens: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CoreLaunch {
    /// The program's arguments, argv[0] first.
    pub args: Vec<String>,
    /// Its environment variables.
    pub env: Vec<(String, String)>,
    /// Preopened directories: (absolute host path, path the program sees).
    pub preopens: Vec<(String, String)>,
    pub entry: Entry,
}

/// How the host drives a core module.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A WASI command: `_start` is called once and runs to the end.
    Command,
    /// A reactor: `_initialize` (if exported), then the export `step` in a
    /// loop until it returns a status >= 0, as QEMU's `wasi_qemu_step`.
    Reactor { step: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub engine: Engine,
    pub mode: Mode,
    pub module: ModuleId,
    pub launch: Launch,
    /// The m-browser that saved it (Cargo.toml's version).
    pub m_browser_version: String,
    /// When it was saved, in seconds since 1970 (UTC).
    pub created_unix_secs: u64,
    /// The tab, or headless session, the program ran in: 12 hex digits.
    pub owner: String,
    /// Saves of this owner's program so far, this one included.
    pub counter: u32,
}

/// A new owner id, for a tab or a headless session: 48 random bits as 12
/// hex digits.
pub fn new_owner_id() -> String {
    format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff)
}

/// Seconds since 1970, now.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A UTC time as `20240610T123456Z` (ISO 8601's basic form, which has no
/// colons, so that it is valid in file names on every system).
pub fn utc_stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs = unix_secs % 86_400;
    // Howard Hinnant's days-to-civil algorithm (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// The file name of a saved state:
/// `<module name>-<UTC time>-<counter>-<owner>.wasmsavestate`, for example
/// `triangle-20240610T123456Z-1-abcdef123456.wasmsavestate`.
pub fn file_name(module_path: &Path, unix_secs: u64, counter: u32, owner: &str) -> String {
    let stem: String = module_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .collect();
    let stem = if stem.is_empty() { "module".to_string() } else { stem };
    format!("{stem}-{}-{counter}-{owner}.{EXTENSION}", utc_stamp(unix_secs))
}

/// A whole saved-state file in memory, for small ones: the layout
/// `StateWriter` writes, with these sections.
pub fn encode(header: &Header, sections: &[([u8; 4], Vec<u8>)]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let header = postcard::to_allocvec(header)?;
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&crc32fast::hash(&header).to_le_bytes());
    for (tag, payload) in sections.iter().chain(std::iter::once(&(TAG_END, Vec::new()))) {
        out.extend_from_slice(tag);
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(payload);
        out.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    }
    Ok(out)
}

/// Writes a saved state. Nothing appears under the final name until
/// `finish` has written and synced the whole file; if writing fails, or
/// the writer is dropped unfinished, the temporary file is removed.
pub struct StateWriter {
    out: Option<BufWriter<File>>,
    tmp: PathBuf,
    path: PathBuf,
}

impl StateWriter {
    pub fn create(path: &Path, header: &Header) -> Result<Self> {
        let tmp = path.with_extension(format!("{EXTENSION}.tmp{}", std::process::id()));
        let file = File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        let mut writer = StateWriter { out: Some(BufWriter::with_capacity(1 << 20, file)), tmp, path: path.to_owned() };
        let header = postcard::to_allocvec(header)?;
        let out = writer.out();
        out.write_all(MAGIC)?;
        out.write_all(&FORMAT_VERSION.to_le_bytes())?;
        out.write_all(&(header.len() as u32).to_le_bytes())?;
        out.write_all(&header)?;
        out.write_all(&crc32fast::hash(&header).to_le_bytes())?;
        Ok(writer)
    }

    fn out(&mut self) -> &mut BufWriter<File> {
        self.out.as_mut().expect("used after finish")
    }

    /// Write a section with this payload.
    pub fn section(&mut self, tag: [u8; 4], payload: &[u8]) -> Result<()> {
        let mut section = self.begin(tag)?;
        section.write_all(payload)?;
        section.end()
    }

    /// Start a section whose payload is too large to build in memory first:
    /// write it through the returned writer, then call its `end`.
    pub fn begin(&mut self, tag: [u8; 4]) -> Result<SectionWriter<'_>> {
        let out = self.out();
        out.write_all(&tag)?;
        let length_at = out.stream_position()?;
        out.write_all(&0u64.to_le_bytes())?;
        Ok(SectionWriter { out, length_at, length: 0, crc: crc32fast::Hasher::new() })
    }

    /// End the file and give it its final name.
    pub fn finish(mut self) -> Result<PathBuf> {
        self.section(TAG_END, &[])?;
        let out = self.out.take().expect("finished twice");
        let file = out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        std::fs::rename(&self.tmp, &self.path)
            .with_context(|| format!("cannot rename {} to {}", self.tmp.display(), self.path.display()))?;
        Ok(self.path.clone())
    }
}

impl Drop for StateWriter {
    fn drop(&mut self) {
        if self.out.take().is_some() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// The payload of a section being written; see `StateWriter::begin`.
pub struct SectionWriter<'a> {
    out: &'a mut BufWriter<File>,
    length_at: u64,
    length: u64,
    crc: crc32fast::Hasher,
}

impl Write for SectionWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.out.write(buf)?;
        self.crc.update(&buf[..n]);
        self.length += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

impl SectionWriter<'_> {
    /// Finish the section: its check value, and its length in front of it.
    pub fn end(self) -> Result<()> {
        self.out.write_all(&self.crc.finalize().to_le_bytes())?;
        let end = self.out.stream_position()?;
        self.out.seek(SeekFrom::Start(self.length_at))?;
        self.out.write_all(&self.length.to_le_bytes())?;
        self.out.seek(SeekFrom::Start(end))?;
        Ok(())
    }
}

/// Reads a saved state: the header at once, then the sections in order.
pub struct StateReader {
    input: BufReader<File>,
    pub header: Header,
    path: PathBuf,
}

impl StateReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let mut input = BufReader::with_capacity(1 << 20, file);
        let shown = path.display();
        let mut magic = [0u8; 8];
        if input.read_exact(&mut magic).is_err() || &magic != MAGIC {
            bail!("{shown} is not a saved state");
        }
        let version = read_u32(&mut input).with_context(|| format!("{shown} is truncated"))?;
        if version > FORMAT_VERSION {
            bail!(
                "{shown} was saved by a newer m-browser (file format {version}; this one reads up to {FORMAT_VERSION})"
            );
        }
        if version == 0 {
            bail!("{shown} is damaged (file format 0)");
        }
        let length = read_u32(&mut input).with_context(|| format!("{shown} is truncated"))?;
        if length > 1 << 20 {
            bail!("{shown} is damaged (header of {length} bytes)");
        }
        let mut header = vec![0u8; length as usize];
        input.read_exact(&mut header).with_context(|| format!("{shown} is truncated"))?;
        let crc = read_u32(&mut input).with_context(|| format!("{shown} is truncated"))?;
        if crc != crc32fast::hash(&header) {
            bail!("{shown} is damaged (its header fails its check)");
        }
        let header: Header =
            postcard::from_bytes(&header).with_context(|| format!("{shown} has a header this m-browser cannot read"))?;
        Ok(StateReader { input, header, path: path.to_owned() })
    }

    /// The next section, or `None` once the end is reached.
    pub fn next(&mut self) -> Result<Option<SectionReader<'_>>> {
        let shown = self.path.display();
        let mut tag = [0u8; 4];
        self.input.read_exact(&mut tag).with_context(|| format!("{shown} is truncated"))?;
        let mut length = [0u8; 8];
        self.input.read_exact(&mut length).with_context(|| format!("{shown} is truncated"))?;
        let section = SectionReader {
            tag,
            input: &mut self.input,
            remaining: u64::from_le_bytes(length),
            crc: crc32fast::Hasher::new(),
            path: &self.path,
        };
        if tag == TAG_END {
            section.end()?;
            return Ok(None);
        }
        Ok(Some(section))
    }
}

/// The payload of one section; `end` checks it against its CRC32.
pub struct SectionReader<'a> {
    pub tag: [u8; 4],
    input: &'a mut BufReader<File>,
    remaining: u64,
    crc: crc32fast::Hasher,
    path: &'a Path,
}

impl Read for SectionReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let max = buf.len().min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.input.read(&mut buf[..max])?;
        if n == 0 && max > 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        self.crc.update(&buf[..n]);
        self.remaining -= n as u64;
        Ok(n)
    }
}

impl SectionReader<'_> {
    /// The tag as text, for messages.
    pub fn name(&self) -> String {
        String::from_utf8_lossy(&self.tag).trim_end_matches('\0').to_string()
    }

    /// The rest of the payload.
    pub fn read_rest(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::with_capacity(self.remaining.min(64 << 20) as usize);
        self.read_to_end(&mut payload)
            .with_context(|| format!("{} is truncated", self.path.display()))?;
        Ok(payload)
    }

    /// Skip what is left of the payload and check the section's CRC32.
    pub fn end(mut self) -> Result<()> {
        let name = self.name();
        std::io::copy(&mut self, &mut std::io::sink())
            .with_context(|| format!("{} is truncated", self.path.display()))?;
        let crc = read_u32(&mut *self.input).with_context(|| format!("{} is truncated", self.path.display()))?;
        if crc != self.crc.finalize() {
            bail!("{} is damaged (section {name} fails its check)", self.path.display());
        }
        Ok(())
    }
}

pub(crate) fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

pub(crate) fn read_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A directory of its own for one test, removed when dropped.
    pub(crate) struct TestDir(pub PathBuf);

    impl TestDir {
        pub(crate) fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("m-browser-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn header() -> Header {
        Header {
            engine: Engine::WasmSnapshot,
            mode: Mode::Hibernate,
            module: ModuleId { path: "/x/prog.wasm".into(), sha256: [7; 32], size: 123 },
            launch: Launch::Core(CoreLaunch {
                args: vec!["prog.wasm".into(), "-v".into()],
                env: vec![("A".into(), "1".into())],
                preopens: vec![("/tmp".into(), "/data".into())],
                entry: Entry::Reactor { step: "step".into() },
            }),
            m_browser_version: "1.0.0".into(),
            created_unix_secs: 1_718_022_896,
            owner: "abcdef123456".into(),
            counter: 1,
        }
    }

    #[test]
    fn names_use_the_basic_utc_form() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
        assert_eq!(utc_stamp(1_718_022_896), "20240610T123456Z");
        assert_eq!(
            file_name(Path::new("/a/b/triangle.wasm"), 1_718_022_896, 1, "abcdef123456"),
            "triangle-20240610T123456Z-1-abcdef123456.wasmsavestate"
        );
        assert_eq!(
            file_name(Path::new("qemu system:x86"), 0, 2, "000000000001"),
            "qemu_system_x86-19700101T000000Z-2-000000000001.wasmsavestate"
        );
        let id = new_owner_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sections_round_trip() {
        let dir = TestDir::new("format");
        let path = dir.0.join("state.wasmsavestate");
        let mut w = StateWriter::create(&path, &header()).unwrap();
        w.section(*b"ONE\0", b"hello").unwrap();
        let mut big = w.begin(*b"BIG\0").unwrap();
        for i in 0..100_000u32 {
            big.write_all(&i.to_le_bytes()).unwrap();
        }
        big.end().unwrap();
        w.section(*b"TWO\0", &[]).unwrap();
        assert!(!path.exists(), "the final name appears only once the file is complete");
        assert_eq!(w.finish().unwrap(), path);

        let mut r = StateReader::open(&path).unwrap();
        assert_eq!(r.header, header());
        let mut s = r.next().unwrap().unwrap();
        assert_eq!((s.name(), s.read_rest().unwrap()), ("ONE".to_string(), b"hello".to_vec()));
        s.end().unwrap();
        let mut s = r.next().unwrap().unwrap();
        assert_eq!(s.name(), "BIG");
        let payload = s.read_rest().unwrap();
        assert_eq!(payload.len(), 400_000);
        assert_eq!(&payload[399_996..], &99_999u32.to_le_bytes());
        s.end().unwrap();
        // A section can be skipped without reading it.
        r.next().unwrap().unwrap().end().unwrap();
        assert!(r.next().unwrap().is_none());
        let leftovers: Vec<_> = std::fs::read_dir(&dir.0).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary file is left behind");
    }

    #[test]
    fn damage_is_detected() {
        let dir = TestDir::new("damage");
        let path = dir.0.join("state.wasmsavestate");
        let mut w = StateWriter::create(&path, &header()).unwrap();
        w.section(*b"ONE\0", &[1u8; 1000]).unwrap();
        w.finish().unwrap();
        let good = std::fs::read(&path).unwrap();

        let read_all = |bytes: &[u8]| -> Result<()> {
            std::fs::write(&path, bytes).unwrap();
            let mut r = StateReader::open(&path)?;
            while let Some(s) = r.next()? {
                s.end()?;
            }
            Ok(())
        };
        read_all(&good).unwrap();

        let mut flipped = good.clone();
        flipped[good.len() - 30] ^= 1;
        let e = read_all(&flipped).unwrap_err().to_string();
        assert!(e.contains("section ONE fails its check"), "{e}");

        let e = read_all(&good[..good.len() - 20]).unwrap_err().to_string();
        assert!(e.contains("truncated"), "{e}");

        let mut newer = good.clone();
        newer[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let e = read_all(&newer).unwrap_err().to_string();
        assert!(e.contains("newer m-browser"), "{e}");

        let e = read_all(b"not a saved state at all").unwrap_err().to_string();
        assert!(e.contains("is not a saved state"), "{e}");

        let mut header_damaged = good.clone();
        header_damaged[20] ^= 0x40;
        let e = read_all(&header_damaged).unwrap_err().to_string();
        assert!(e.contains("header fails its check"), "{e}");
    }

    #[test]
    fn unfinished_files_leave_nothing() {
        let dir = TestDir::new("unfinished");
        let path = dir.0.join("state.wasmsavestate");
        let mut w = StateWriter::create(&path, &header()).unwrap();
        w.section(*b"ONE\0", b"x").unwrap();
        drop(w);
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }
}
