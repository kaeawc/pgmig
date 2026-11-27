//! A dependency graph over plan steps and its deterministic topological sort.
//!
//! Each step declares, as [`Object`]s, what it creates, drops, alters and references ([`Deps`]).
//! Edges follow from those declarations alone:
//!
//! - a step that `requires` an object runs after the step that creates it;
//! - a step that `releases` an object (stops referencing it, or drops something that referenced
//!   it) runs before the step that drops it;
//! - a step that drops an object runs before a step that creates an object of the same identity;
//! - steps that alter the same object run in phase order.
//!
//! Among steps whose dependencies are met, the lowest `(phase, index)` runs first, so the order is
//! stable and unrelated steps keep the planner's fixed phases and the diff's order.
//!
//! A cycle is an error ([`Cycle`]) naming the steps on it. Cycles are not broken here: an object
//! that can take part in one (a foreign key) is planned as a separate step after the tables it
//! joins, so the graph never sees a cycle between tables.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use pgmig_model::QualName;

/// An object a step creates, drops, alters or references. Adding a kind of object means adding a
/// variant here and declaring it in the [`Deps`] of the steps that touch it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Object {
    Schema(String),
    Table(QualName),
    /// A type by its `pg_type` name. Creating a table creates its row type of the same name.
    Type(QualName),
    Column(QualName, String),
    /// The type of a column, apart from the column itself: a type change and the foreign keys
    /// over the column (on either side) alter it, so those keys go before the change and come
    /// back after, without being ordered against the column's other constraints.
    ColumnType(QualName, String),
    /// A sequence, standalone or (by its implicit name) an identity column's.
    Sequence(QualName),
    /// A table constraint by its table and name. A foreign key requires the constraint whose
    /// index it uses; a table's drop drops its constraints.
    Constraint(QualName, String),
    /// An index by its schema-qualified name: one no constraint owns, or a primary key, unique or
    /// exclusion constraint's (named like the constraint). A foreign key requires the index it
    /// uses; a table's drop drops its indexes.
    Index(QualName),
    /// A relation name (`pg_class`): tables, sequences and indexes share one namespace per schema.
    /// A step creating or dropping a [`Object::Table`], [`Object::Sequence`] or [`Object::Index`]
    /// creates or drops the relation of its name too, so dropping index `foo` runs before creating
    /// table `foo`.
    Relation(QualName),
}

/// What one step does to which objects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deps {
    pub creates: Vec<Object>,
    pub drops: Vec<Object>,
    /// Objects altered in place; steps altering the same object keep their phase order.
    pub alters: Vec<Object>,
    /// Objects that must exist when the step runs.
    pub requires: Vec<Object>,
    /// Objects referenced before the step and not after it: they may only be dropped afterwards.
    pub releases: Vec<Object>,
}

/// A step to order: its fixed phase (the tie-break) and its dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub phase: u8,
    pub deps: Deps,
}

/// The indices of steps forming a cycle, in execution order (each must run before the next, and
/// the last before the first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle(pub Vec<usize>);

/// The indices of `nodes` in an order that satisfies every edge.
pub fn sort(nodes: &[Node]) -> Result<Vec<usize>, Cycle> {
    let edges = edges(nodes);
    let mut preds: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); nodes.len()];
    let mut succs: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); nodes.len()];
    for (from, to) in edges {
        succs[from].insert(to);
        preds[to].insert(from);
    }

    let mut indegree: Vec<usize> = preds.iter().map(BTreeSet::len).collect();
    let key = |i: usize| Reverse((nodes[i].phase, i));
    let mut ready: BinaryHeap<Reverse<(u8, usize)>> = (0..nodes.len()).filter(|&i| indegree[i] == 0).map(key).collect();
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(Reverse((_, i))) = ready.pop() {
        order.push(i);
        for &s in &succs[i] {
            indegree[s] -= 1;
            if indegree[s] == 0 {
                ready.push(key(s));
            }
        }
    }
    if order.len() == nodes.len() {
        return Ok(order);
    }
    Err(find_cycle(&preds, &indegree))
}

fn edges(nodes: &[Node]) -> BTreeSet<(usize, usize)> {
    let mut creators: BTreeMap<&Object, Vec<usize>> = BTreeMap::new();
    let mut droppers: BTreeMap<&Object, Vec<usize>> = BTreeMap::new();
    let mut alterers: BTreeMap<&Object, Vec<usize>> = BTreeMap::new();
    for (i, n) in nodes.iter().enumerate() {
        n.deps.creates.iter().for_each(|o| creators.entry(o).or_default().push(i));
        n.deps.drops.iter().for_each(|o| droppers.entry(o).or_default().push(i));
        n.deps.alters.iter().for_each(|o| alterers.entry(o).or_default().push(i));
    }
    let all = |map: &BTreeMap<&Object, Vec<usize>>, o: &Object| map.get(o).cloned().unwrap_or_default();

    let mut edges = BTreeSet::new();
    for (i, n) in nodes.iter().enumerate() {
        for o in &n.deps.requires {
            edges.extend(all(&creators, o).into_iter().map(|c| (c, i)));
        }
        for o in &n.deps.releases {
            edges.extend(all(&droppers, o).into_iter().map(|d| (i, d)));
        }
        for o in &n.deps.creates {
            edges.extend(all(&droppers, o).into_iter().map(|d| (d, i)));
        }
    }
    for steps in alterers.values_mut() {
        steps.sort_by_key(|&i| (nodes[i].phase, i));
        edges.extend(steps.windows(2).map(|w| (w[0], w[1])));
    }
    edges.retain(|(a, b)| a != b);
    edges
}

/// Every unsorted node has an unsorted predecessor, so walking predecessors must revisit a node.
fn find_cycle(preds: &[BTreeSet<usize>], indegree: &[usize]) -> Cycle {
    let stuck = |i: usize| indegree[i] > 0;
    let mut path = Vec::new();
    let mut seen = BTreeMap::new();
    let mut at = (0..indegree.len()).find(|&i| stuck(i)).unwrap_or(0);
    while !seen.contains_key(&at) {
        seen.insert(at, path.len());
        path.push(at);
        at = preds[at].iter().copied().find(|&p| stuck(p)).unwrap_or(at);
    }
    let mut cycle = path.split_off(seen[&at]);
    cycle.reverse();
    Cycle(cycle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(name: &str) -> Object {
        Object::Table(QualName::new("public", name))
    }

    fn node(phase: u8, creates: &[&str], requires: &[&str]) -> Node {
        Node {
            phase,
            deps: Deps {
                creates: creates.iter().map(|n| obj(n)).collect(),
                requires: requires.iter().map(|n| obj(n)).collect(),
                ..Deps::default()
            },
        }
    }

    #[test]
    fn unrelated_nodes_sort_by_phase_then_index() {
        let nodes = vec![node(2, &[], &[]), node(1, &[], &[]), node(2, &[], &[]), node(0, &[], &[])];
        assert_eq!(sort(&nodes), Ok(vec![3, 1, 0, 2]));
    }

    #[test]
    fn requires_wins_over_phase_and_index() {
        let nodes = vec![node(1, &["a"], &["b"]), node(1, &["b"], &[])];
        assert_eq!(sort(&nodes), Ok(vec![1, 0]));
    }

    #[test]
    fn releases_run_before_the_drop() {
        let drop_b = Node { phase: 8, deps: Deps { drops: vec![obj("b")], ..Deps::default() } };
        let drop_a = Node { phase: 8, deps: Deps { drops: vec![obj("a")], releases: vec![obj("b")], ..Deps::default() } };
        assert_eq!(sort(&[drop_b, drop_a]), Ok(vec![1, 0]));
    }

    #[test]
    fn drop_runs_before_a_create_of_the_same_object() {
        let create = node(1, &["a"], &[]);
        let drop = Node { phase: 8, deps: Deps { drops: vec![obj("a")], ..Deps::default() } };
        assert_eq!(sort(&[create, drop]), Ok(vec![1, 0]));
    }

    #[test]
    fn alters_of_one_object_keep_phase_order() {
        let alter = |phase| Node { phase, deps: Deps { alters: vec![obj("t")], ..Deps::default() } };
        // The phase-5 step is held back by a create; the phase-6 step must still wait for it.
        let mut late = alter(5);
        late.deps.requires.push(obj("x"));
        let nodes = vec![alter(6), late, node(9, &["x"], &[])];
        assert_eq!(sort(&nodes), Ok(vec![2, 1, 0]));
    }

    #[test]
    fn a_cycle_is_an_error_naming_its_steps() {
        let nodes = vec![node(0, &[], &[]), node(1, &["a"], &["c"]), node(1, &["b"], &["a"]), node(1, &["c"], &["b"])];
        let Err(Cycle(steps)) = sort(&nodes) else { panic!("expected a cycle") };
        let mut sorted = steps.clone();
        sorted.sort();
        assert_eq!(sorted, vec![1, 2, 3]);
        // Execution order: each step's creation is required by the next one.
        for w in steps.windows(2) {
            assert!(nodes[w[1]].deps.requires.iter().any(|o| nodes[w[0]].deps.creates.contains(o)));
        }
    }
}
