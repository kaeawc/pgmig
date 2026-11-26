//! The migration directory: its files, their order, and the sum file that pins them.
//!
//! # Files
//!
//! A migration is a file named `<version>_<description>.sql` directly in the directory. The version
//! is a non-empty string of ASCII digits (a timestamp such as `20261126103000` by convention); the
//! description is the non-empty rest of the name. Files are ordered by version as a number, so
//! `9_a.sql` comes before `10_b.sql`; two files with the same number (`1_a.sql`, `01_b.sql`) are an
//! error, as is a `.sql` file whose name does not follow the pattern. Everything else is ignored:
//! files without the `.sql` extension, names starting with `.` (editor lock and swap files), the
//! sum file and subdirectories.
//!
//! A new migration's version is the UTC time as `YYYYMMDDHHMMSS`, or one more than the newest
//! file's when the clock does not give a greater one ([`next_version`]), so it always sorts last;
//! its description is the name it was given made of `[a-z0-9_]` ([`sanitize_description`]).
//!
//! # Sum file
//!
//! [`SUM_FILE`] (`pgmig.sum`) sits next to the migrations:
//!
//! ```text
//! h1:<directory hash>
//! 20261125093000_create_users.sql h1:<file hash>
//! 20261126101500_add_orders.sql h1:<file hash>
//! ```
//!
//! Hashes are SHA-256, printed in standard base64 with padding. A file's hash covers the previous
//! file's hash (its 32 raw bytes; nothing for the first file), the file name, a zero byte and the
//! file's bytes, so editing, renaming, inserting or deleting a file changes the hash of every file
//! after it, and two branches that each add a file both rewrite the last line: merging them is a
//! version-control conflict, never a silent union. The directory hash covers the file lines exactly
//! as rendered (`<file> h1:<hash>\n` each), so a hand-edited line is caught even when the files it
//! names are unchanged.
//!
//! Files are hashed byte for byte as they are on disk: no newline or encoding normalization. A
//! checkout that rewrites line endings (`core.autocrlf`) changes the hashes, so mark migration
//! files `-text` or `eol=lf` in `.gitattributes` when that applies. The sum file itself is parsed
//! with or without `\r` before each newline.
//!
//! # Revisions table
//!
//! [`revisions`]: the table in the target database recording what has been applied;
//! [`history`]: what it leaves to apply.

pub mod history;
pub mod revisions;

use std::cmp::Ordering;
use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};

/// The sum file's name in a migration directory.
pub const SUM_FILE: &str = "pgmig.sum";

const H1: &str = "h1:";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path}: {reason}")]
    Name { path: PathBuf, reason: NameError },
    #[error("{second} has the same version as {first}")]
    DuplicateVersion { first: PathBuf, second: PathBuf },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Why a `.sql` file name is not a migration name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("file name is not UTF-8")]
    NotUtf8,
    #[error("migration file names are <version>_<description>.sql; the version must be digits")]
    NoVersion,
    #[error("migration file names are <version>_<description>.sql; the description is missing")]
    NoDescription,
}

/// One migration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// The file name, `<version>_<description>.sql`.
    pub name: String,
    pub version: String,
    pub description: String,
    /// The contents, exactly as on disk.
    pub bytes: Vec<u8>,
}

impl File {
    /// A migration named `name` holding `bytes`.
    pub fn new(name: &str, bytes: impl Into<Vec<u8>>) -> std::result::Result<File, NameError> {
        let (version, description) = parse_name(name)?;
        Ok(File { name: name.to_owned(), version: version.to_owned(), description: description.to_owned(), bytes: bytes.into() })
    }

    /// The file's own hash (base64): SHA-256 over its name, a zero byte and its bytes, without the
    /// chain of the sum file. The revisions table records it, so a file inserted before applied
    /// ones (non-linear order) leaves their recorded hashes valid. For the first file it equals
    /// its sum-file line.
    pub fn hash(&self) -> String {
        BASE64.encode(chain(None, &self.name, &self.bytes))
    }
}

/// Split `<version>_<description>.sql` into its version and description.
pub fn parse_name(name: &str) -> std::result::Result<(&str, &str), NameError> {
    // A control character (a newline, say) would make the sum file unreadable.
    if name.chars().any(char::is_control) {
        return Err(NameError::NoVersion);
    }
    let stem = name.strip_suffix(".sql").unwrap_or(name);
    let digits = stem.bytes().take_while(u8::is_ascii_digit).count();
    let (version, rest) = stem.split_at(digits);
    if version.is_empty() {
        return Err(NameError::NoVersion);
    }
    match rest.strip_prefix('_') {
        Some(d) if !d.is_empty() => Ok((version, d)),
        Some(_) => Err(NameError::NoDescription),
        // `20261126.sql` has no description; `2026x_a.sql` has a version that is not all digits.
        None if rest.is_empty() => Err(NameError::NoDescription),
        None => Err(NameError::NoVersion),
    }
}

/// Versions as numbers: leading zeros do not count.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// A migration directory: its files in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dir {
    pub path: PathBuf,
    files: Vec<File>,
}

impl Dir {
    /// Read the migrations directly in `path`, in version order.
    pub fn open(path: &Path) -> Result<Dir> {
        let io = |source| Error::Io { path: path.to_owned(), source };
        let mut files = Vec::new();
        for entry in std::fs::read_dir(path).map_err(io)? {
            let entry = entry.map_err(io)?;
            let p = entry.path();
            let name = entry.file_name();
            let lossy = name.to_string_lossy();
            if !p.extension().is_some_and(|e| e == "sql") || lossy.starts_with('.') || p.is_dir() {
                continue;
            }
            let Some(name) = name.to_str() else { return Err(Error::Name { path: p, reason: NameError::NotUtf8 }) };
            let bytes = std::fs::read(&p).map_err(|source| Error::Io { path: p.clone(), source })?;
            files.push(File::new(name, bytes).map_err(|reason| Error::Name { path: p.clone(), reason })?);
        }
        Dir::from_files(path, files)
    }

    /// A directory at `path` holding `files` (in any order), without touching the file system.
    pub fn from_files(path: &Path, mut files: Vec<File>) -> Result<Dir> {
        files.sort_by(|a, b| compare_versions(&a.version, &b.version).then_with(|| a.name.cmp(&b.name)));
        if let Some(w) = files.windows(2).find(|w| compare_versions(&w[0].version, &w[1].version).is_eq()) {
            return Err(Error::DuplicateVersion { first: path.join(&w[0].name), second: path.join(&w[1].name) });
        }
        Ok(Dir { path: path.to_owned(), files })
    }

    pub fn files(&self) -> &[File] {
        &self.files
    }

    /// Where this directory's sum file is.
    pub fn sum_path(&self) -> PathBuf {
        self.path.join(SUM_FILE)
    }

    /// The sum file for the files as they are now.
    pub fn sum(&self) -> SumFile {
        let mut prev: Option<[u8; 32]> = None;
        let lines = self
            .files
            .iter()
            .map(|f| {
                let h = chain(prev.as_ref(), &f.name, &f.bytes);
                prev = Some(h);
                SumLine { file: f.name.clone(), hash: BASE64.encode(h) }
            })
            .collect();
        SumFile::from_lines(lines)
    }

    /// The sum file on disk; `None` when there is none.
    pub fn read_sum(&self) -> std::result::Result<Option<SumFile>, ValidateError> {
        let path = self.sum_path();
        match std::fs::read(&path) {
            Ok(bytes) => {
                let text =
                    String::from_utf8(bytes).map_err(|_| ValidateError::Malformed { path: path.clone(), line: 1, reason: "not UTF-8" })?;
                SumFile::parse(&text).map(Some).map_err(|e| ValidateError::Malformed { path, line: e.line, reason: e.reason })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ValidateError::Io { path, source }),
        }
    }

    /// Write [`Dir::sum`] to the sum file, unless it already holds exactly that. Returns the sum
    /// file and whether it was written.
    pub fn write_sum(&self) -> Result<(SumFile, bool)> {
        let sum = self.sum();
        let path = self.sum_path();
        let text = sum.render();
        if std::fs::read(&path).is_ok_and(|old| old == text.as_bytes()) {
            return Ok((sum, false));
        }
        std::fs::write(&path, text).map_err(|source| Error::Io { path, source })?;
        Ok((sum, true))
    }
}

/// The hash of a file following the file whose hash is `prev`.
fn chain(prev: Option<&[u8; 32]>, name: &str, bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    if let Some(p) = prev {
        h.update(p);
    }
    h.update(name.as_bytes());
    h.update([0]);
    h.update(bytes);
    h.finalize().into()
}

/// A line of the sum file after the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumLine {
    pub file: String,
    /// The chained hash in base64, without `h1:`.
    pub hash: String,
}

impl fmt::Display for SumLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {H1}{}", self.file, self.hash)
    }
}

/// The contents of a sum file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumFile {
    /// The directory hash in base64, without `h1:`, as written on line one.
    pub dir_hash: String,
    pub lines: Vec<SumLine>,
}

/// Why a sum file could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {reason}")]
pub struct ParseError {
    /// 1-based.
    pub line: u32,
    pub reason: &'static str,
}

impl SumFile {
    /// A sum file for `lines`, with their directory hash.
    pub fn from_lines(lines: Vec<SumLine>) -> SumFile {
        SumFile { dir_hash: dir_hash(&lines), lines }
    }

    pub fn parse(text: &str) -> std::result::Result<SumFile, ParseError> {
        let mut rows = text.split_inclusive('\n').map(|l| l.strip_suffix('\n').unwrap_or(l)).map(|l| l.strip_suffix('\r').unwrap_or(l));
        let err = |line: usize, reason| ParseError { line: u32::try_from(line).unwrap_or(u32::MAX), reason };
        let first = rows.next().ok_or(err(1, "empty; expected h1:<directory hash>"))?;
        let dir_hash = first.strip_prefix(H1).filter(|h| is_hash(h)).ok_or(err(1, "expected h1:<directory hash>"))?;
        let mut lines = Vec::new();
        for (i, row) in rows.enumerate() {
            let n = i + 2;
            let (file, hash) = row.rsplit_once(' ').ok_or(err(n, "expected <file> h1:<hash>"))?;
            let hash = hash.strip_prefix(H1).filter(|h| is_hash(h)).ok_or(err(n, "expected <file> h1:<hash>"))?;
            if file.is_empty() {
                return Err(err(n, "expected <file> h1:<hash>"));
            }
            lines.push(SumLine { file: file.to_owned(), hash: hash.to_owned() });
        }
        Ok(SumFile { dir_hash: dir_hash.to_owned(), lines })
    }

    /// The file's text: line one, then one line per file, each ending in `\n`.
    pub fn render(&self) -> String {
        let mut s = format!("{H1}{}\n", self.dir_hash);
        for l in &self.lines {
            s.push_str(&format!("{l}\n"));
        }
        s
    }

    /// Whether line one matches the file lines below it.
    pub fn is_consistent(&self) -> bool {
        self.dir_hash == dir_hash(&self.lines)
    }
}

fn is_hash(s: &str) -> bool {
    BASE64.decode(s).is_ok_and(|b| b.len() == 32)
}

fn dir_hash(lines: &[SumLine]) -> String {
    let mut h = Sha256::new();
    for l in lines {
        h.update(format!("{l}\n").as_bytes());
    }
    BASE64.encode(h.finalize())
}

/// What is wrong with a migration directory and its sum file. `file` is the path of the migration
/// file the problem is about; `path` the sum file's.
#[derive(Debug, thiserror::Error)]
pub enum ValidateError {
    #[error("{path}: no sum file; run `pgmig migrate hash` to create it")]
    MissingSum { path: PathBuf },
    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path}:{line}: {reason}")]
    Malformed { path: PathBuf, line: u32, reason: &'static str },
    #[error("{path}: the sum file was edited by hand: its directory hash does not match its lines; run `pgmig migrate hash` to rewrite it")]
    SumEdited { path: PathBuf },
    #[error("{file}: listed in the sum file but deleted from the directory")]
    Deleted { file: PathBuf },
    #[error(
        "{file}: added before {before}, which is already in the sum file, so the hash of every later file changes; give it a later version, or run `pgmig migrate hash` if that order is intended"
    )]
    Inserted { file: PathBuf, before: String },
    #[error("{file}: not in the sum file; run `pgmig migrate hash`")]
    NotHashed { file: PathBuf },
    #[error("{file}: listed in the sum file where {expected} is in the directory; the files were renamed or reordered")]
    Reordered { file: PathBuf, expected: String },
    #[error("{file}: edited after it was hashed (its hash in the sum file does not match)")]
    Edited { file: PathBuf },
}

impl ValidateError {
    /// The file the problem is in: a migration file, or the sum file.
    pub fn path(&self) -> &Path {
        match self {
            ValidateError::MissingSum { path }
            | ValidateError::Io { path, .. }
            | ValidateError::Malformed { path, .. }
            | ValidateError::SumEdited { path } => path,
            ValidateError::Deleted { file }
            | ValidateError::Inserted { file, .. }
            | ValidateError::NotHashed { file }
            | ValidateError::Reordered { file, .. }
            | ValidateError::Edited { file } => file,
        }
    }
}

/// Check `dir` against its sum file on disk.
pub fn validate(dir: &Dir) -> std::result::Result<(), ValidateError> {
    match dir.read_sum()? {
        None => Err(ValidateError::MissingSum { path: dir.sum_path() }),
        Some(sum) => check(dir, &sum),
    }
}

/// Check `dir` against `sum`, reporting the first problem: a hand-edited sum file, then a deleted
/// file, then an added one, then the first file whose hash changed.
pub fn check(dir: &Dir, sum: &SumFile) -> std::result::Result<(), ValidateError> {
    if !sum.is_consistent() {
        return Err(ValidateError::SumEdited { path: dir.sum_path() });
    }
    let actual = dir.sum();
    let file = |name: &str| dir.path.join(name);
    let listed = |name: &str| sum.lines.iter().any(|l| l.file == name);
    let present = |name: &str| actual.lines.iter().any(|l| l.file == name);
    if let Some(l) = sum.lines.iter().find(|l| !present(&l.file)) {
        return Err(ValidateError::Deleted { file: file(&l.file) });
    }
    if let Some(i) = actual.lines.iter().position(|l| !listed(&l.file)) {
        let added = file(&actual.lines[i].file);
        return Err(match actual.lines[i + 1..].iter().find(|l| listed(&l.file)) {
            Some(next) => ValidateError::Inserted { file: added, before: next.file.clone() },
            None => ValidateError::NotHashed { file: added },
        });
    }
    // Same files on both sides now.
    for (want, got) in sum.lines.iter().zip(&actual.lines) {
        if want.file != got.file {
            return Err(ValidateError::Reordered { file: file(&want.file), expected: got.file.clone() });
        }
        if want.hash != got.hash {
            return Err(ValidateError::Edited { file: file(&got.file) });
        }
    }
    Ok(())
}

/// The version of a new migration written at `now`: the UTC timestamp `YYYYMMDDHHMMSS`, or, when
/// that is not greater than `last` (a clock behind the newest file, or two files in one second),
/// `last + 1`. Either way it sorts after every existing file.
pub fn next_version(last: Option<&str>, now: std::time::SystemTime) -> String {
    let stamp = timestamp(now);
    match last {
        Some(l) if compare_versions(&stamp, l) != Ordering::Greater => increment(l),
        _ => stamp,
    }
}

/// `t` in UTC as `YYYYMMDDHHMMSS`; a time before 1970 counts as 1970.
pub fn timestamp(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (proleptic Gregorian), from Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A version (digits) plus one, without leading zeros.
fn increment(v: &str) -> String {
    let mut digits: Vec<u8> = v.trim_start_matches('0').bytes().collect();
    let mut i = digits.len();
    loop {
        if i == 0 {
            digits.insert(0, b'1');
            break;
        }
        i -= 1;
        if digits[i] == b'9' {
            digits[i] = b'0';
        } else {
            digits[i] += 1;
            break;
        }
    }
    String::from_utf8(digits).unwrap_or_default()
}

/// A migration description made from `name`: ASCII letters lowercased, digits kept, every run of
/// anything else one `_`, none at either end. `None` when nothing is left.
pub fn sanitize_description(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(timestamp(at(0)), "19700101000000");
        assert_eq!(timestamp(at(1_795_689_000)), "20261126103000");
        assert_eq!(timestamp(at(1_709_251_199)), "20240229235959");
        assert_eq!(timestamp(at(951_868_800)), "20000301000000");
        assert_eq!(timestamp(std::time::UNIX_EPOCH - std::time::Duration::from_secs(5)), "19700101000000");
    }

    #[test]
    fn next_version_is_after_the_last() {
        let now = at(1_795_689_000);
        assert_eq!(next_version(None, now), "20261126103000");
        assert_eq!(next_version(Some("20261125000000"), now), "20261126103000");
        assert_eq!(next_version(Some("9"), now), "20261126103000");
        // The same second, or a clock behind the newest file: one more than it.
        assert_eq!(next_version(Some("20261126103000"), now), "20261126103001");
        assert_eq!(next_version(Some("99991231235959"), now), "99991231235960");
        assert_eq!(next_version(Some("99999999999999"), now), "100000000000000");
        assert_eq!(next_version(Some("0099999999999999"), now), "100000000000000");
        assert_eq!(increment("0"), "1");
        assert_eq!(increment("009"), "10");
    }

    #[test]
    fn descriptions_are_sanitized() {
        assert_eq!(sanitize_description("add_users").as_deref(), Some("add_users"));
        assert_eq!(sanitize_description("Add Users!").as_deref(), Some("add_users"));
        assert_eq!(sanitize_description("  --orders.v2--  ").as_deref(), Some("orders_v2"));
        assert_eq!(sanitize_description("café crème").as_deref(), Some("caf_cr_me"));
        assert_eq!(sanitize_description("__a__b__").as_deref(), Some("a_b"));
        assert_eq!(sanitize_description("2fa").as_deref(), Some("2fa"));
        assert_eq!(sanitize_description(""), None);
        assert_eq!(sanitize_description("!!! ---"), None);
        let name = format!("{}_{}.sql", next_version(None, at(0)), sanitize_description("x y").expect("name"));
        assert_eq!(parse_name(&name), Ok(("19700101000000", "x_y")));
    }

    fn dir(files: &[(&str, &str)]) -> Dir {
        let files = files.iter().map(|(n, t)| File::new(n, t.as_bytes()).expect("name")).collect();
        Dir::from_files(Path::new("m"), files).expect("dir")
    }

    fn names(d: &Dir) -> Vec<&str> {
        d.files().iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn control_characters_are_not_in_names() {
        for name in ["2026_a\nb.sql", "2026_a\tb.sql", "2026_a\u{7f}.sql", "2026_a\r.sql"] {
            assert!(parse_name(name).is_err(), "{name:?}");
            assert!(File::new(name, "").is_err(), "{name:?}");
        }
        assert!(parse_name("2026_a b.sql").is_ok());
    }

    #[test]
    fn names_split_into_version_and_description() {
        assert_eq!(parse_name("20261126103000_create_users.sql"), Ok(("20261126103000", "create_users")));
        assert_eq!(parse_name("1_a b.sql"), Ok(("1", "a b")));
        assert_eq!(parse_name("create_users.sql"), Err(NameError::NoVersion));
        assert_eq!(parse_name("_x.sql"), Err(NameError::NoVersion));
        assert_eq!(parse_name("2026x_a.sql"), Err(NameError::NoVersion));
        assert_eq!(parse_name("2026-11-26_a.sql"), Err(NameError::NoVersion));
        assert_eq!(parse_name("20261126.sql"), Err(NameError::NoDescription));
        assert_eq!(parse_name("20261126_.sql"), Err(NameError::NoDescription));
    }

    #[test]
    fn versions_order_as_numbers() {
        let d = dir(&[("10_b.sql", ""), ("9_a.sql", ""), ("0011_c.sql", ""), ("20261126103000_d.sql", "")]);
        assert_eq!(names(&d), ["9_a.sql", "10_b.sql", "0011_c.sql", "20261126103000_d.sql"]);
        assert_eq!(compare_versions("007", "7"), Ordering::Equal);
        assert_eq!(compare_versions("0", "000"), Ordering::Equal);
    }

    #[test]
    fn duplicate_versions_are_rejected() {
        for (a, b) in [("1_a.sql", "1_b.sql"), ("1_a.sql", "01_b.sql")] {
            let files = vec![File::new(a, "").expect("a"), File::new(b, "").expect("b")];
            let err = Dir::from_files(Path::new("m"), files).expect_err("duplicate");
            assert!(matches!(err, Error::DuplicateVersion { .. }), "{err}");
            assert!(err.to_string().contains("has the same version as"), "{err}");
        }
    }

    #[test]
    fn hashes_chain() {
        let base = dir(&[("1_a.sql", "A"), ("2_b.sql", "B"), ("3_c.sql", "C")]);
        let edited = dir(&[("1_a.sql", "A!"), ("2_b.sql", "B"), ("3_c.sql", "C")]);
        let (s, e) = (base.sum(), edited.sum());
        // Editing the first file changes every line and the directory hash.
        for (a, b) in s.lines.iter().zip(&e.lines) {
            assert_ne!(a.hash, b.hash);
        }
        assert_ne!(s.dir_hash, e.dir_hash);
        // Editing the last changes only the last line.
        let last = dir(&[("1_a.sql", "A"), ("2_b.sql", "B"), ("3_c.sql", "C\n")]).sum();
        assert_eq!(s.lines[..2], last.lines[..2]);
        assert_ne!(s.lines[2], last.lines[2]);
        // The name is hashed: the same bytes under another name hash differently.
        assert_ne!(dir(&[("1_a.sql", "A")]).sum().lines[0].hash, dir(&[("1_x.sql", "A")]).sum().lines[0].hash);
        // Bytes are hashed as they are: CRLF is not LF.
        assert_ne!(dir(&[("1_a.sql", "A\n")]).sum(), dir(&[("1_a.sql", "A\r\n")]).sum());
    }

    #[test]
    fn hash_is_sha256_of_name_and_bytes_after_the_previous_hash() {
        let sum = dir(&[("1_a.sql", "A"), ("2_b.sql", "B")]).sum();
        let first: [u8; 32] = Sha256::digest(b"1_a.sql\0A").into();
        assert_eq!(sum.lines[0].hash, BASE64.encode(first));
        let mut second = first.to_vec();
        second.extend_from_slice(b"2_b.sql\0B");
        assert_eq!(sum.lines[1].hash, BASE64.encode(Sha256::digest(&second)));
        let lines = format!("1_a.sql h1:{}\n2_b.sql h1:{}\n", sum.lines[0].hash, sum.lines[1].hash);
        assert_eq!(sum.dir_hash, BASE64.encode(Sha256::digest(lines.as_bytes())));
    }

    #[test]
    fn sum_render_parse_round_trip() {
        let sum = dir(&[("1_a b.sql", "A"), ("2_b.sql", "B")]).sum();
        let text = sum.render();
        let mut rows = text.lines();
        assert!(rows.next().is_some_and(|l| l.starts_with("h1:") && l.len() == 47), "{text}");
        assert!(rows.next().is_some_and(|l| l.starts_with("1_a b.sql h1:")), "{text}");
        assert!(text.ends_with('\n'));
        assert_eq!(SumFile::parse(&text), Ok(sum.clone()));
        assert_eq!(SumFile::parse(&text.replace('\n', "\r\n")), Ok(sum.clone()));
        assert_eq!(SumFile::parse(text.trim_end()), Ok(sum));
        let empty = dir(&[]).sum();
        assert_eq!(SumFile::parse(&empty.render()), Ok(empty));
    }

    #[test]
    fn malformed_sum_files_name_the_line() {
        let good = dir(&[("1_a.sql", "A")]).sum().render();
        let hash = good.lines().next().expect("line one");
        for (text, line) in [
            ("".to_owned(), 1),
            ("h2:abc\n".to_owned(), 1),
            ("h1:not-base64\n".to_owned(), 1),
            (format!("{hash}\n1_a.sql\n"), 2),
            (format!("{hash}\n1_a.sql h1:AAAA\n"), 2),
            (format!("{hash}\n h1:{}\n", &hash[3..]), 2),
            (format!("{hash}\n\n"), 2),
        ] {
            let err = SumFile::parse(&text).expect_err(&text);
            assert_eq!(err.line, line, "{text:?}: {err}");
        }
    }

    fn checked(base: &Dir, now: &Dir) -> std::result::Result<(), ValidateError> {
        check(now, &base.sum())
    }

    #[test]
    fn unchanged_directory_passes() {
        let d = dir(&[("1_a.sql", "A"), ("2_b.sql", "B")]);
        checked(&d, &d).expect("valid");
        checked(&dir(&[]), &dir(&[])).expect("empty");
    }

    #[test]
    fn edited_file_is_named() {
        let base = dir(&[("1_a.sql", "A"), ("2_b.sql", "B"), ("3_c.sql", "C")]);
        let now = dir(&[("1_a.sql", "A"), ("2_b.sql", "B edited"), ("3_c.sql", "C")]);
        let err = checked(&base, &now).expect_err("edited");
        assert!(matches!(&err, ValidateError::Edited { file } if file == Path::new("m/2_b.sql")), "{err}");
    }

    #[test]
    fn inserted_file_is_named_with_the_file_after_it() {
        let base = dir(&[("1_a.sql", "A"), ("3_c.sql", "C")]);
        let now = dir(&[("1_a.sql", "A"), ("2_b.sql", "B"), ("3_c.sql", "C")]);
        let err = checked(&base, &now).expect_err("inserted");
        assert!(matches!(&err, ValidateError::Inserted { file, before } if file == Path::new("m/2_b.sql") && before == "3_c.sql"), "{err}");
        assert!(err.to_string().contains("the hash of every later file changes"), "{err}");
        // Hashing it in changes the later line too.
        assert_ne!(base.sum().lines[1].hash, now.sum().lines[2].hash);
    }

    #[test]
    fn deleted_file_is_named() {
        let base = dir(&[("1_a.sql", "A"), ("2_b.sql", "B"), ("3_c.sql", "C")]);
        let now = dir(&[("1_a.sql", "A"), ("3_c.sql", "C")]);
        let err = checked(&base, &now).expect_err("deleted");
        assert!(matches!(&err, ValidateError::Deleted { file } if file == Path::new("m/2_b.sql")), "{err}");
    }

    #[test]
    fn appended_file_needs_a_rehash() {
        let base = dir(&[("1_a.sql", "A")]);
        let now = dir(&[("1_a.sql", "A"), ("2_b.sql", "B")]);
        let err = checked(&base, &now).expect_err("not hashed");
        assert!(matches!(&err, ValidateError::NotHashed { file } if file == Path::new("m/2_b.sql")), "{err}");
        check(&now, &now.sum()).expect("valid after rehash");
    }

    #[test]
    fn renamed_file_is_reported() {
        let base = dir(&[("1_a.sql", "A"), ("2_b.sql", "B")]);
        let now = dir(&[("1_a.sql", "A"), ("2_x.sql", "B")]);
        let err = checked(&base, &now).expect_err("renamed");
        assert!(matches!(&err, ValidateError::Deleted { file } if file == Path::new("m/2_b.sql")), "{err}");
    }

    #[test]
    fn hand_edited_sum_is_caught() {
        let d = dir(&[("1_a.sql", "A"), ("2_b.sql", "B")]);
        let mut sum = d.sum();
        // Pointing a line at the edited file without recomputing line one.
        sum.lines[1].hash = dir(&[("1_a.sql", "A"), ("2_b.sql", "B!")]).sum().lines[1].hash.clone();
        let err = check(&d, &sum).expect_err("hand edited");
        assert!(matches!(err, ValidateError::SumEdited { .. }), "{err}");
        // Removing a line by hand.
        let mut sum = d.sum();
        sum.lines.pop();
        assert!(matches!(check(&d, &sum), Err(ValidateError::SumEdited { .. })));
        // Recomputing line one does not hide an edited file.
        let edited = dir(&[("1_a.sql", "A"), ("2_b.sql", "B!")]);
        let err = check(&edited, &SumFile::from_lines(d.sum().lines)).expect_err("edited");
        assert!(matches!(&err, ValidateError::Edited { file } if file == Path::new("m/2_b.sql")), "{err}");
    }

    /// Two branches that each add a migration rewrite the same last line differently (and line
    /// one), so merging their sum files is a text conflict rather than a clean union.
    #[test]
    fn parallel_additions_conflict_on_the_last_line() {
        let base = [("20261101000000_a.sql", "A"), ("20261102000000_b.sql", "B")];
        let with = |extra: (&'static str, &'static str)| {
            let mut files = base.to_vec();
            files.push(extra);
            dir(&files).sum().render()
        };
        let base_sum = dir(&base).sum().render();
        let ours = with(("20261110000000_x.sql", "X"));
        let theirs = with(("20261111000000_y.sql", "Y"));
        let last = |s: &str| s.lines().last().map(str::to_owned).expect("a line");
        let first = |s: &str| s.lines().next().map(str::to_owned).expect("a line");
        let (b, o, t) = (base_sum.lines().collect::<Vec<_>>(), ours.lines().collect::<Vec<_>>(), theirs.lines().collect::<Vec<_>>());
        // Both keep the base's file lines and append one line after them.
        assert_eq!((o.len(), t.len()), (b.len() + 1, b.len() + 1));
        assert_eq!(o[1..b.len()], b[1..]);
        assert_eq!(t[1..b.len()], b[1..]);
        // ...and both change line one: each side edits the same lines, a conflict.
        assert_ne!(first(&ours), first(&theirs));
        assert_ne!(first(&ours), first(&base_sum));
        assert_ne!(last(&ours), last(&theirs));
        assert_ne!(last(&ours), last(&base_sum));
        assert_ne!(last(&theirs), last(&base_sum));
        // Resolving by keeping both added lines leaves a sum file that does not validate.
        let both = dir(&[base[0], base[1], ("20261110000000_x.sql", "X"), ("20261111000000_y.sql", "Y")]);
        let union = format!("{}{}\n", ours, last(&theirs));
        let err = check(&both, &SumFile::parse(&union).expect("parse")).expect_err("union");
        assert!(matches!(err, ValidateError::SumEdited { .. }), "{err}");
    }

    /// A directory on disk, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(files: &[(&str, &[u8])]) -> TempDir {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!("pgmig-migrate-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&path).expect("create temp dir");
            for (name, bytes) in files {
                std::fs::write(path.join(name), bytes).expect("write file");
            }
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn open_reads_migrations_and_ignores_everything_else() {
        let t = TempDir::new(&[
            ("2_b.sql", b"B"),
            ("1_a.sql", b"A"),
            ("README.md", b"notes"),
            (".1_a.sql.swp", b"x"),
            (".#2_b.sql", b"x"),
            (SUM_FILE, b"junk"),
        ]);
        std::fs::create_dir(t.0.join("3_sub.sql")).expect("subdir");
        std::fs::create_dir(t.0.join("old")).expect("subdir");
        std::fs::write(t.0.join("old").join("0_z.sql"), "Z").expect("nested");
        let d = Dir::open(&t.0).expect("open");
        assert_eq!(names(&d), ["1_a.sql", "2_b.sql"]);
        assert_eq!(d.files()[0].bytes, b"A");
    }

    #[test]
    fn open_names_the_malformed_file() {
        let t = TempDir::new(&[("1_a.sql", b"A"), ("create_users.sql", b"B")]);
        let err = Dir::open(&t.0).expect_err("bad name");
        assert!(matches!(&err, Error::Name { path, reason: NameError::NoVersion } if path == &t.0.join("create_users.sql")), "{err}");
        let t = TempDir::new(&[("1_a.sql", b"A"), ("001_b.sql", b"B")]);
        let err = Dir::open(&t.0).expect_err("duplicate");
        assert!(err.to_string().contains("001_b.sql") && err.to_string().contains("1_a.sql"), "{err}");
        let missing = t.0.join("nope");
        assert!(matches!(Dir::open(&missing), Err(Error::Io { .. })));
    }

    #[test]
    fn write_sum_then_validate() {
        let t = TempDir::new(&[("1_a.sql", b"A\r\n"), ("2_b.sql", b"B")]);
        let d = Dir::open(&t.0).expect("open");
        assert!(matches!(validate(&d), Err(ValidateError::MissingSum { .. })));
        let (sum, written) = d.write_sum().expect("write");
        assert!(written);
        assert_eq!(std::fs::read_to_string(d.sum_path()).expect("read"), sum.render());
        validate(&d).expect("valid");
        assert!(!d.write_sum().expect("rewrite").1, "unchanged sum file is not rewritten");
        std::fs::write(d.sum_path(), "nonsense\n").expect("clobber");
        assert!(matches!(validate(&d), Err(ValidateError::Malformed { line: 1, .. })));
    }
}
