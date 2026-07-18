//! Louvain community detection — in-house, RNG-free, deterministic.
//!
//! # Why in-house
//!
//! There is no Rust clustering crate worth depending on here. `rustworkx-core`
//! does **not** ship Louvain at any version (upstream Qiskit/rustworkx#1141 has
//! been open since 2024-03-14 with no PR — an earlier draft of the M2b design
//! claimed otherwise and was wrong). The alternatives (`leiden-rs`,
//! `single-clustering`, `fa-leiden-cd`, `rune-leiden`) are young,
//! single-maintainer, low-adoption and silent on determinism — which is the one
//! property this module exists to guarantee. Depending on one of them would
//! trade a known-absent dependency for an unaudited one at the core of the
//! clustering path. So: ~250 lines here, no new dependency, no C FFI, no RNG.
//!
//! # Why the API takes an edge list rather than a graph
//!
//! [`louvain`] takes a node count plus `&[(usize, usize, f64)]`, never a graph
//! object. This is load-bearing rather than stylistic: in every graph library,
//! node-index assignment order *is* the non-determinism. A caller who hands us a
//! pre-built graph has already baked its insertion order in, and no amount of
//! care inside this module can recover from that. Taking indices forces the
//! caller to canonicalise first — in production, by sorting on `entities.name`
//! (`UNIQUE (workspace_id, name)`), **never** `entities.id`, which is
//! `gen_random_uuid()` and therefore stable within one database but not across a
//! rebuild.
//!
//! # Where determinism comes from
//!
//! Construction, not seeding. There is no RNG in this module at all. Three
//! mechanisms, all of them required:
//!
//! 1. Nodes are visited in fixed index order `0..n`.
//! 2. Candidate communities are iterated in ascending id, via [`BTreeMap`].
//!    A `HashMap` would be wrong: Rust randomises `HashMap` iteration order per
//!    process, so it would silently destroy determinism in a way that only shows
//!    up as a flaky test months later.
//! 3. The gain comparison is strictly-greater-plus-epsilon, so an exact tie
//!    always loses to the incumbent community.

use std::collections::BTreeMap;

/// Gain must beat the incumbent by more than this to trigger a move.
///
/// Two jobs: it makes exact ties lose to the incumbent (determinism), and it
/// makes each accepted move increase modularity by a strictly positive amount,
/// which is what guarantees the local-moving loop terminates.
const EPSILON: f64 = 1e-12;

/// A partition of `0..n` into communities.
///
/// Stored in **canonical form**: each community's members are sorted ascending,
/// and the communities themselves are sorted by their member lists. This is the
/// whole point of the type. Raw label vectors are *not* comparable across runs —
/// two partitions can be identical as set-partitions while assigning different
/// integer labels, so a label-vector comparison reports spurious inequality.
/// Canonicalising at construction means `==` on a `Partition` is set-partition
/// equality, and callers cannot forget to do it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Partition {
    communities: Vec<Vec<usize>>,
}

impl Partition {
    /// Build a canonical partition from a raw label vector (`labels[node]`).
    ///
    /// Label values are arbitrary and need not be dense; only the induced
    /// grouping matters.
    pub fn from_labels(labels: &[usize]) -> Self {
        let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (node, &label) in labels.iter().enumerate() {
            groups.entry(label).or_default().push(node);
        }
        let mut communities: Vec<Vec<usize>> = groups.into_values().collect();
        // Members arrive ascending (we push in node order); communities are
        // sorted by member list so the whole structure is order-independent.
        communities.sort();
        Self { communities }
    }

    /// The communities, in canonical order, each with members sorted ascending.
    pub fn communities(&self) -> &[Vec<usize>] {
        &self.communities
    }

    /// Number of communities.
    pub fn len(&self) -> usize {
        self.communities.len()
    }

    /// True when the partition has no communities (i.e. an empty graph).
    pub fn is_empty(&self) -> bool {
        self.communities.is_empty()
    }

    /// The index into [`Self::communities`] holding `node`, or `None` if `node`
    /// is out of range for this partition.
    pub fn community_of(&self, node: usize) -> Option<usize> {
        self.communities
            .iter()
            .position(|c| c.binary_search(&node).is_ok())
    }
}

/// A weighted undirected graph, in the form the algorithm actually wants.
///
/// Cross edges live in `adj` in **both** directions; self-loops live separately
/// in `self_loops` (stored once, undoubled). Keeping them apart matters because
/// a self-loop contributes `2w` to a node's degree but never moves between
/// communities, so it must not appear among a node's links-to-communities.
struct Graph {
    /// `adj[i]` = `(neighbour, weight)`, ascending by neighbour, no self-loops.
    adj: Vec<Vec<(usize, f64)>>,
    /// Undoubled self-loop weight per node.
    self_loops: Vec<f64>,
    /// Weighted degree: cross weights plus twice the self-loop.
    degrees: Vec<f64>,
    /// Total edge weight `m` (each undirected edge counted once).
    total: f64,
}

impl Graph {
    fn build(node_count: usize, edges: &[(usize, usize, f64)]) -> Self {
        // BTreeMap, not HashMap: the adjacency lists must come out in ascending
        // neighbour order regardless of edge insertion order. Parallel edges are
        // merged by summing, which also makes a duplicated edge harmless.
        let mut acc: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); node_count];
        let mut self_loops = vec![0.0; node_count];

        for &(a, b, w) in edges {
            debug_assert!(
                a < node_count && b < node_count,
                "edge endpoint out of range"
            );
            if a >= node_count || b >= node_count {
                continue;
            }
            if a == b {
                self_loops[a] += w;
            } else {
                *acc[a].entry(b).or_insert(0.0) += w;
                *acc[b].entry(a).or_insert(0.0) += w;
            }
        }

        let adj: Vec<Vec<(usize, f64)>> = acc
            .into_iter()
            .map(|m| m.into_iter().collect::<Vec<_>>())
            .collect();

        let degrees: Vec<f64> = (0..node_count)
            .map(|i| adj[i].iter().map(|&(_, w)| w).sum::<f64>() + 2.0 * self_loops[i])
            .collect();

        let total = degrees.iter().sum::<f64>() / 2.0;

        Self {
            adj,
            self_loops,
            degrees,
            total,
        }
    }

    fn node_count(&self) -> usize {
        self.adj.len()
    }

    /// One Louvain level: move nodes between communities until no move improves
    /// modularity. Returns a *dense* label per node (`0..k`).
    fn local_moving(&self) -> Vec<usize> {
        let n = self.node_count();
        let two_m = 2.0 * self.total;

        // Start from singletons.
        let mut comm: Vec<usize> = (0..n).collect();
        let mut sum_tot: Vec<f64> = self.degrees.clone();

        if two_m <= 0.0 {
            return dense_labels(&comm);
        }

        loop {
            let mut moved = false;

            // Mechanism 1: fixed index order. Never a queue, never a set.
            for i in 0..n {
                let ci = comm[i];
                let ki = self.degrees[i];

                // Mechanism 2: ascending community id. BTreeMap, never HashMap.
                let mut links: BTreeMap<usize, f64> = BTreeMap::new();
                for &(j, w) in &self.adj[i] {
                    *links.entry(comm[j]).or_insert(0.0) += w;
                }

                // Provisionally remove `i` from its community, so the incumbent
                // is scored on the same footing as every alternative.
                sum_tot[ci] -= ki;

                let mut best_comm = ci;
                let mut best_gain =
                    links.get(&ci).copied().unwrap_or(0.0) - sum_tot[ci] * ki / two_m;

                for (&c, &w_ic) in &links {
                    if c == ci {
                        continue;
                    }
                    let gain = w_ic - sum_tot[c] * ki / two_m;
                    // Mechanism 3: strictly greater, by more than EPSILON. An
                    // exact tie leaves the node where it is.
                    if gain > best_gain + EPSILON {
                        best_gain = gain;
                        best_comm = c;
                    }
                }

                sum_tot[best_comm] += ki;
                comm[i] = best_comm;
                if best_comm != ci {
                    moved = true;
                }
            }

            if !moved {
                break;
            }
        }

        dense_labels(&comm)
    }

    /// Collapse each community into a single node.
    ///
    /// The aggregated graph's self-loop for community `c` is the total weight of
    /// edges internal to `c` plus the self-loops its members already carried;
    /// its cross weight to `d` is the total weight between the two. Both are
    /// preserved exactly, which is why modularity computed at any level agrees
    /// with modularity computed on the original graph.
    fn aggregate(&self, labels: &[usize], community_count: usize) -> Graph {
        let mut acc: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); community_count];
        // Internal cross weight, accumulated twice (once per direction).
        let mut internal_doubled = vec![0.0; community_count];
        let mut self_loops = vec![0.0; community_count];

        for i in 0..self.node_count() {
            let ci = labels[i];
            self_loops[ci] += self.self_loops[i];
            for &(j, w) in &self.adj[i] {
                let cj = labels[j];
                if ci == cj {
                    internal_doubled[ci] += w;
                } else {
                    *acc[ci].entry(cj).or_insert(0.0) += w;
                }
            }
        }

        for c in 0..community_count {
            self_loops[c] += internal_doubled[c] / 2.0;
        }

        let adj: Vec<Vec<(usize, f64)>> = acc
            .into_iter()
            .map(|m| m.into_iter().collect::<Vec<_>>())
            .collect();

        let degrees: Vec<f64> = (0..community_count)
            .map(|c| adj[c].iter().map(|&(_, w)| w).sum::<f64>() + 2.0 * self_loops[c])
            .collect();
        let total = degrees.iter().sum::<f64>() / 2.0;

        Graph {
            adj,
            self_loops,
            degrees,
            total,
        }
    }
}

/// Renumber arbitrary labels to `0..k`, assigning new ids in ascending order of
/// old id. Deterministic by construction (BTreeMap), which matters because these
/// ids become node indices of the next level's graph.
fn dense_labels(labels: &[usize]) -> Vec<usize> {
    let mut map: BTreeMap<usize, usize> = BTreeMap::new();
    for &l in labels {
        let next = map.len();
        map.entry(l).or_insert(next);
    }
    labels.iter().map(|l| map[l]).collect()
}

/// Cluster a canonically-ordered edge list into communities.
///
/// `node_count` is the number of nodes; nodes are `0..node_count`. `edges` are
/// undirected `(a, b, weight)`; parallel edges are summed, `a == b` is treated
/// as a self-loop, and out-of-range endpoints are ignored (they are a caller
/// bug, and are `debug_assert!`ed).
///
/// **The caller must canonicalise first.** Index assignment order is the entire
/// source of non-determinism; see the module docs.
pub fn louvain(node_count: usize, edges: &[(usize, usize, f64)]) -> Partition {
    if node_count == 0 {
        return Partition::from_labels(&[]);
    }

    let mut graph = Graph::build(node_count, edges);
    // Maps each ORIGINAL node to its community at the current level.
    let mut node_to_comm: Vec<usize> = (0..node_count).collect();

    loop {
        let labels = graph.local_moving();
        let community_count = labels.iter().copied().max().map_or(0, |m| m + 1);

        // No aggregation happened: this level is a fixed point, so is the next.
        if community_count == graph.node_count() {
            break;
        }

        for c in node_to_comm.iter_mut() {
            *c = labels[*c];
        }

        graph = graph.aggregate(&labels, community_count);

        if graph.node_count() <= 1 {
            break;
        }
    }

    Partition::from_labels(&node_to_comm)
}

/// Newman–Girvan modularity of `partition` on the graph given by `edges`.
///
/// `Q = Σ_c [ in_c / 2m − (tot_c / 2m)² ]`, where `in_c` counts each internal
/// edge twice (as `A_ij + A_ji`) and a self-loop twice (as `A_ii`), and `tot_c`
/// is the summed weighted degree of `c`'s members.
///
/// Exists both as the acceptance criterion for [`louvain`] (karate club must
/// score ≈ 0.4188) and as a useful measure in its own right.
pub fn modularity(node_count: usize, edges: &[(usize, usize, f64)], partition: &Partition) -> f64 {
    if node_count == 0 {
        return 0.0;
    }

    let mut label = vec![usize::MAX; node_count];
    for (c, members) in partition.communities().iter().enumerate() {
        for &m in members {
            if m < node_count {
                label[m] = c;
            }
        }
    }

    let k = partition.len();
    let mut degrees = vec![0.0; node_count];
    let mut internal = vec![0.0; k];
    let mut total = 0.0;

    for &(a, b, w) in edges {
        if a >= node_count || b >= node_count {
            continue;
        }
        total += w;
        if a == b {
            degrees[a] += 2.0 * w;
        } else {
            degrees[a] += w;
            degrees[b] += w;
        }
        // Both a self-loop (A_ii = 2w) and an internal cross edge
        // (A_ij + A_ji = 2w) contribute 2w.
        if label[a] != usize::MAX && label[a] == label[b] {
            internal[label[a]] += 2.0 * w;
        }
    }

    if total <= 0.0 {
        return 0.0;
    }

    let two_m = 2.0 * total;
    let mut tot = vec![0.0; k];
    for node in 0..node_count {
        if label[node] != usize::MAX {
            tot[label[node]] += degrees[node];
        }
    }

    (0..k)
        .map(|c| internal[c] / two_m - (tot[c] / two_m).powi(2))
        .sum()
}
