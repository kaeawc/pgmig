//! Changing a migration directory and rewriting its sum file to match.
//!
//! Each function takes the directory as it was read (and, normally, checked against its sum file)
//! and writes the sum file for those files with its change only: a file that appeared on disk in
//! the meantime is left unhashed, so the next check reports it instead of hashing it unseen.

use std::path::Path;
use std::time::SystemTime;

use crate::{Dir, Error, File, Result, SumFile, next_version};

/// Delete the migration `name` of `dir` and write the sum file of the files left.
pub fn remove(dir: &Dir, name: &str) -> Result<SumFile> {
    let path = dir.path.join(name);
    std::fs::remove_file(&path).map_err(|source| Error::Io { path, source })?;
    Ok(dir.without(&[name]).write_sum()?.0)
}

/// Read the migration `name` of `dir` from disk again (it was edited) and write the sum file with
/// its new contents. Returns the file as it is now and the sum file.
pub fn rehash_file(dir: &Dir, name: &str) -> Result<(File, SumFile)> {
    let path = dir.path.join(name);
    let bytes = std::fs::read(&path).map_err(|source| Error::Io { path: path.clone(), source })?;
    let file = File::new(name, bytes).map_err(|reason| Error::Name { path, reason })?;
    let mut files = dir.without(&[name]).files().to_vec();
    files.push(file.clone());
    let (sum, _) = Dir::from_files(&dir.path, files)?.write_sum()?;
    Ok((file, sum))
}

/// A file [`rebase`] moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    /// The file name before, and after.
    pub from: String,
    pub to: String,
    pub from_version: String,
    pub to_version: String,
}

/// Where [`rebase`] would move the files `names` of `dir`, without touching the file system: after
/// every other file, in their order, each with the next version ([`next_version`] at `now`) and its
/// description kept. Nothing moves when they already come after every other file.
pub fn rebase_moves(dir: &Dir, names: &[&str], now: SystemTime) -> Vec<Move> {
    let rest = dir.without(names);
    let moved: Vec<&File> = dir.files().iter().filter(|f| names.contains(&f.name.as_str())).collect();
    let Some(first) = moved.first() else { return Vec::new() };
    let mut last = rest.files().last().map(|f| f.version.clone());
    if last.as_deref().is_none_or(|l| crate::compare_versions(&first.version, l).is_gt()) {
        return Vec::new();
    }
    moved
        .into_iter()
        .map(|f| {
            let v = next_version(last.as_deref(), now);
            last = Some(v.clone());
            Move { from: f.name.clone(), to: format!("{v}_{}.sql", f.description), from_version: f.version.clone(), to_version: v }
        })
        .collect()
}

/// Move the files `names` of `dir` after every other file ([`rebase_moves`]): move them, then
/// write the sum file. A file already at a new name is never overwritten. The caller holds the
/// [`DirLock`](crate::DirLock) of the directory from before `dir` was read.
pub fn rebase(dir: &Dir, names: &[&str], now: SystemTime) -> Result<(Vec<Move>, SumFile)> {
    let moves = rebase_moves(dir, names, now);
    for m in &moves {
        let to = dir.path.join(&m.to);
        if to.exists() {
            return Err(Error::Exists { path: to });
        }
    }
    let mut synced = false;
    let mut files = dir.without(names).files().to_vec();
    for f in dir.files().iter().filter(|f| names.contains(&f.name.as_str())) {
        match moves.iter().find(|m| m.from == f.name) {
            Some(m) => {
                rename(&dir.path, &m.from, &m.to)?;
                synced = true;
                files.push(File::new(&m.to, f.bytes.clone()).map_err(|reason| Error::Name { path: dir.path.join(&m.to), reason })?);
            }
            None => files.push(f.clone()),
        }
    }
    if synced {
        sync_dir(&dir.path)?;
    }
    let (sum, _) = Dir::from_files(&dir.path, files)?.write_sum()?;
    Ok((moves, sum))
}

/// Move `from` to `to` in `dir` without overwriting: a `to` that exists is [`Error::Exists`].
fn rename(dir: &Path, from: &str, to: &str) -> Result<()> {
    match crate::move_no_overwrite(&dir.join(from), &dir.join(to)) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(Error::Exists { path: dir.join(to) }),
        r => r.map_err(|source| Error::Io { path: dir.join(from), source }),
    }
}

fn sync_dir(dir: &Path) -> Result<()> {
    crate::sync_parent(&dir.join("x")).map_err(|source| Error::Io { path: dir.to_owned(), source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{check, validate};

    fn at(secs: u64) -> SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    fn dir(files: &[(&str, &str)]) -> Dir {
        let files = files.iter().map(|(n, t)| File::new(n, t.as_bytes()).expect("name")).collect();
        Dir::from_files(Path::new("m"), files).expect("dir")
    }

    #[test]
    fn find_by_name_version_or_unique_prefix() {
        let d = dir(&[("20261101000000_a.sql", ""), ("20261102000000_b.sql", ""), ("20261201000000_c.sql", ""), ("7_d.sql", "")]);
        let name = |k: &str| d.find(k).map(|f| f.name.clone());
        assert_eq!(name("20261102000000_b.sql").as_deref().ok(), Some("20261102000000_b.sql"));
        assert_eq!(name("some/dir/20261102000000_b.sql").as_deref().ok(), Some("20261102000000_b.sql"));
        assert_eq!(name("20261101000000").as_deref().ok(), Some("20261101000000_a.sql"));
        assert_eq!(name("007").as_deref().ok(), Some("7_d.sql"));
        assert_eq!(name("202612").as_deref().ok(), Some("20261201000000_c.sql"));
        let err = d.find("202611").expect_err("ambiguous");
        assert!(matches!(&err, Error::Ambiguous { candidates, .. } if candidates.len() == 2), "{err}");
        assert!(err.to_string().contains("20261101000000_a.sql, 20261102000000_b.sql"), "{err}");
        for k in ["9", "x_a.sql", "", "20261101000000_z.sql", "a"] {
            assert!(matches!(d.find(k), Err(Error::NotFound { .. })), "{k:?}");
        }
    }

    #[test]
    fn rebase_moves_out_of_order_files_to_the_end() {
        let d = dir(&[
            ("20261101000000_a.sql", ""),
            ("20261103000000_mine.sql", ""),
            ("20261104000000_theirs.sql", ""),
            ("20261105000000_theirs2.sql", ""),
        ]);
        let moves = rebase_moves(&d, &["20261103000000_mine.sql"], at(0));
        // The clock (1970) is behind the newest file: one more than it.
        assert_eq!(
            moves,
            [Move {
                from: "20261103000000_mine.sql".into(),
                to: "20261105000001_mine.sql".into(),
                from_version: "20261103000000".into(),
                to_version: "20261105000001".into()
            }]
        );
        // Two files keep their order, each after the one before.
        let two = rebase_moves(&d, &["20261104000000_theirs.sql", "20261101000000_a.sql"], at(0));
        assert_eq!(two.iter().map(|m| m.to.as_str()).collect::<Vec<_>>(), ["20261105000001_a.sql", "20261105000002_theirs.sql"]);
        // With the clock ahead: its timestamp, then one more.
        let now = rebase_moves(&d, &["20261101000000_a.sql", "20261103000000_mine.sql"], at(1_795_689_000));
        assert_eq!(now.iter().map(|m| m.to.as_str()).collect::<Vec<_>>(), ["20261126103000_a.sql", "20261126103001_mine.sql"]);
        // Already last, or nothing named: nothing moves.
        assert!(rebase_moves(&d, &["20261105000000_theirs2.sql"], at(0)).is_empty());
        assert!(rebase_moves(&d, &["20261104000000_theirs.sql", "20261105000000_theirs2.sql"], at(0)).is_empty());
        assert!(rebase_moves(&d, &[], at(0)).is_empty());
        assert!(rebase_moves(&dir(&[("1_a.sql", "")]), &["1_a.sql"], at(0)).is_empty());
    }

    /// A directory on disk, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(files: &[(&str, &str)]) -> TempDir {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!("pgmig-maintain-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&path).expect("create temp dir");
            for (name, text) in files {
                std::fs::write(path.join(name), text).expect("write file");
            }
            let t = TempDir(path);
            Dir::open(&t.0).expect("open").write_sum().expect("hash");
            t
        }

        fn names(&self) -> Vec<String> {
            let mut v: Vec<String> =
                std::fs::read_dir(&self.0).expect("read").map(|e| e.expect("entry").file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn remove_rehashes() {
        let t = TempDir::new(&[("1_a.sql", "A"), ("2_b.sql", "B"), ("3_c.sql", "C")]);
        let d = Dir::open(&t.0).expect("open");
        let sum = remove(&d, "2_b.sql").expect("remove");
        assert_eq!(sum.lines.iter().map(|l| l.file.as_str()).collect::<Vec<_>>(), ["1_a.sql", "3_c.sql"]);
        assert_eq!(t.names(), ["1_a.sql", "3_c.sql", "pgmig.sum"]);
        validate(&Dir::open(&t.0).expect("open")).expect("valid");
    }

    #[test]
    fn rehash_file_takes_the_new_contents_only() {
        let t = TempDir::new(&[("1_a.sql", "A"), ("2_b.sql", "B")]);
        let d = Dir::open(&t.0).expect("open");
        std::fs::write(t.0.join("1_a.sql"), "A edited").expect("edit");
        // A file that appeared meanwhile is not hashed unseen.
        std::fs::write(t.0.join("3_c.sql"), "C").expect("add");
        let (file, sum) = rehash_file(&d, "1_a.sql").expect("rehash");
        assert_eq!(file.bytes, b"A edited");
        assert_eq!(sum.lines.len(), 2);
        let now = Dir::open(&t.0).expect("open");
        assert!(matches!(validate(&now), Err(crate::ValidateError::NotHashed { .. })));
        check(&now.without(&["3_c.sql"]), &sum).expect("the rest is valid");
    }

    #[test]
    fn rebase_renames_and_rehashes() {
        let t = TempDir::new(&[("20261101000000_a.sql", "A"), ("20261103000000_mine.sql", "M"), ("20261104000000_theirs.sql", "T")]);
        let d = Dir::open(&t.0).expect("open");
        let (moves, _) = rebase(&d, &["20261103000000_mine.sql"], at(0)).expect("rebase");
        assert_eq!(moves.len(), 1);
        assert_eq!(t.names(), ["20261101000000_a.sql", "20261104000000_theirs.sql", "20261104000001_mine.sql", "pgmig.sum"]);
        assert_eq!(std::fs::read(t.0.join("20261104000001_mine.sql")).expect("read"), b"M");
        validate(&Dir::open(&t.0).expect("open")).expect("valid");
    }

    #[test]
    fn rebase_never_overwrites() {
        let t = TempDir::new(&[("20261101000000_a.sql", "A"), ("20261103000000_mine.sql", "M"), ("20261104000000_theirs.sql", "T")]);
        let d = Dir::open(&t.0).expect("open");
        // Written after the directory was read: rebase must not overwrite it.
        std::fs::write(t.0.join("20261104000001_mine.sql"), "other").expect("squat");
        let err = rebase(&d, &["20261103000000_mine.sql"], at(0)).expect_err("exists");
        assert!(matches!(&err, Error::Exists { path } if path.ends_with("20261104000001_mine.sql")), "{err}");
        assert!(t.0.join("20261103000000_mine.sql").is_file());
    }

    #[test]
    fn write_atomic_leaves_no_temporary_file() {
        let t = TempDir::new(&[("1_a.sql", "A")]);
        crate::write_atomic(&t.0.join("pgmig.sum"), b"x").expect("write");
        assert_eq!(t.names(), ["1_a.sql", "pgmig.sum"]);
        assert_eq!(std::fs::read(t.0.join("pgmig.sum")).expect("read"), b"x");
        assert!(crate::write_atomic(&t.0.join("nope").join("f"), b"x").is_err());
    }
}
