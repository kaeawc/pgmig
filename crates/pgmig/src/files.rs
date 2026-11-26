//! Finding the SQL files of a source and ordering them: `-- pgmig:import <path>` makes a file
//! load after the files it names; everything else keeps lexical order.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// A SQL file to execute: the path as the user would recognise it, and its split contents.
pub(crate) struct Loaded {
    pub path: PathBuf,
    pub file: pgmig_sql::File,
}

/// The `.sql` files directly in a directory in lexical order, or the path itself when it is a file.
pub(crate) fn sql_files(path: &Path) -> Result<Vec<PathBuf>> {
    let io = |source| Error::Io { path: path.to_owned(), source };
    if !path.is_dir() {
        return Ok(vec![path.to_owned()]);
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path).map_err(io)? {
        let p = entry.map_err(io)?.path();
        if p.extension().is_some_and(|e| e == "sql") {
            files.push(p);
        }
    }
    if files.is_empty() {
        return Err(Error::NoSql { path: path.to_owned() });
    }
    files.sort();
    Ok(files)
}

struct Node {
    path: PathBuf,
    file: Option<pgmig_sql::File>,
}

/// Read and split the files at `path` and everything they import, in load order.
pub(crate) fn load_files(path: &Path) -> Result<Vec<Loaded>> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut index: HashMap<PathBuf, usize> = HashMap::new();
    let mut deps: Vec<Vec<usize>> = Vec::new();

    let mut intern = |p: PathBuf, nodes: &mut Vec<Node>, deps: &mut Vec<Vec<usize>>| -> Result<usize> {
        let canon = std::fs::canonicalize(&p).map_err(|source| Error::Io { path: p.clone(), source })?;
        if let Some(&i) = index.get(&canon) {
            return Ok(i);
        }
        nodes.push(Node { path: p, file: None });
        deps.push(Vec::new());
        index.insert(canon, nodes.len() - 1);
        Ok(nodes.len() - 1)
    };

    for p in sql_files(path)? {
        intern(p, &mut nodes, &mut deps)?;
    }
    let mut i = 0;
    while i < nodes.len() {
        let p = nodes[i].path.clone();
        let text = std::fs::read_to_string(&p).map_err(|source| Error::Io { path: p.clone(), source })?;
        let file = pgmig_sql::split(&text).map_err(|source| Error::Split { path: p.clone(), source })?;
        // Imports go above the first statement (one directly above it attaches to that statement).
        let start = file.statements.first().map(|s| s.pos);
        let all = file.directives.iter().chain(file.statements.iter().flat_map(|s| &s.directives));
        let mut imports: Vec<_> = all.filter(|d| d.name == "import").collect();
        imports.sort_by_key(|d| d.pos);
        if let Some(d) = imports.iter().find(|d| start.is_some_and(|s| d.pos >= s)) {
            return Err(Error::ImportPlacement { path: p, pos: d.pos });
        }
        for d in imports {
            if d.args.is_empty() {
                return Err(Error::ImportArgs { path: p, pos: d.pos });
            }
            let target = p.parent().unwrap_or(Path::new("")).join(&d.args);
            std::fs::metadata(&target).map_err(|source| match source.kind() {
                std::io::ErrorKind::NotFound => Error::ImportMissing { path: p.clone(), pos: d.pos, target: d.args.clone() },
                _ => Error::Io { path: target.clone(), source },
            })?;
            let files = sql_files(&target).map_err(|e| match e {
                Error::NoSql { .. } => Error::ImportEmpty { path: p.clone(), pos: d.pos, target: d.args.clone() },
                e => e,
            })?;
            for f in files {
                let j = intern(f, &mut nodes, &mut deps)?;
                if !deps[i].contains(&j) {
                    deps[i].push(j);
                }
            }
        }
        nodes[i].file = Some(file);
        i += 1;
    }

    // Order by the path as found, not the canonical one, so a symlinked entry keeps its place.
    let keys: Vec<PathBuf> = nodes.iter().map(|n| n.path.clone()).collect();
    match order(&keys, &deps) {
        Ok(ord) => {
            let mut slots: Vec<Option<Node>> = nodes.into_iter().map(Some).collect();
            Ok(ord
                .into_iter()
                .map(|i| {
                    let n = slots[i].take().expect("each node once");
                    Loaded { path: n.path, file: n.file.expect("split above") }
                })
                .collect())
        }
        Err(cycle) => {
            let mut names: Vec<String> = cycle.iter().map(|&i| nodes[i].path.display().to_string()).collect();
            names.push(names[0].clone());
            Err(Error::ImportCycle { cycle: names.join(" -> ") })
        }
    }
}

/// A dependency order of nodes `0..keys.len()`: dependencies first, and among the nodes that are
/// ready the one with the smallest key, so files without imports stay in key order. On a cycle,
/// returns its nodes in cycle order.
fn order(keys: &[PathBuf], deps: &[Vec<usize>]) -> std::result::Result<Vec<usize>, Vec<usize>> {
    let n = keys.len();
    let mut waiting: Vec<usize> = deps.iter().map(Vec::len).collect();
    let mut users: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ds) in deps.iter().enumerate() {
        for &d in ds {
            users[d].push(i);
        }
    }
    let mut ready: BTreeSet<(&PathBuf, usize)> = (0..n).filter(|&i| waiting[i] == 0).map(|i| (&keys[i], i)).collect();
    let mut out = Vec::with_capacity(n);
    while let Some(first) = ready.pop_first() {
        let i = first.1;
        out.push(i);
        for &u in &users[i] {
            waiting[u] -= 1;
            if waiting[u] == 0 {
                ready.insert((&keys[u], u));
            }
        }
    }
    if out.len() == n {
        return Ok(out);
    }
    // Every node left over still waits on another left-over node, so walking dependencies from
    // any of them ends in a cycle.
    let stuck = |i: usize| waiting[i] > 0;
    let start = (0..n).filter(|&i| stuck(i)).min_by_key(|&i| &keys[i]).expect("a node is stuck");
    let mut walk = vec![start];
    loop {
        let last = *walk.last().expect("non-empty");
        let next = deps[last].iter().copied().filter(|&d| stuck(d)).min_by_key(|&d| &keys[d]).expect("stuck node has a stuck dependency");
        if let Some(at) = walk.iter().position(|&w| w == next) {
            return Err(walk.split_off(at));
        }
        walk.push(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn no_imports_keeps_key_order() {
        let k = keys(&["a", "b", "c"]);
        assert_eq!(order(&k, &[vec![], vec![], vec![]]), Ok(vec![0, 1, 2]));
    }

    #[test]
    fn dependencies_first_then_smallest_ready() {
        // a imports c; b is free. Ready: b, c -> b first, then c, then a.
        let k = keys(&["a", "b", "c"]);
        assert_eq!(order(&k, &[vec![2], vec![], vec![]]), Ok(vec![1, 2, 0]));
    }

    #[test]
    fn diamond() {
        let k = keys(&["a", "b", "c", "d"]);
        assert_eq!(order(&k, &[vec![1, 2], vec![3], vec![3], vec![]]), Ok(vec![3, 1, 2, 0]));
    }

    #[test]
    fn cycle_is_reported_in_order() {
        let k = keys(&["a", "b", "c", "z"]);
        // z is fine; a -> b -> c -> b is a cycle reachable from a.
        assert_eq!(order(&k, &[vec![1], vec![2], vec![1], vec![]]), Err(vec![1, 2]));
        assert_eq!(order(&k, &[vec![0], vec![], vec![], vec![]]), Err(vec![0]));
    }
}
