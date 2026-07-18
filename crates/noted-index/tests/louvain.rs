//! Louvain community detection — correctness and determinism.
//!
//! Three tests, none of which substitutes for another:
//!
//! 1. `karate_club_modularity_matches_reference` — a deterministic but WRONG
//!    clusterer is useless, and no determinism test can detect that failure.
//! 2. `twenty_runs_on_identical_input_yield_one_partition` — the cheap guarantee.
//! 3. `twenty_runs_on_permuted_input_canonicalised_yield_the_baseline_partition`
//!    — the load-bearing one. Node/edge insertion order is the actual source of
//!    non-determinism in Louvain, so this is the test that catches real bugs.
//!    It only fires on graphs where order genuinely matters: a symmetric graph
//!    (a ring of cliques, say) survives permutation by luck and would make this
//!    test vacuous. Karate is asymmetric and is KNOWN to break under permutation
//!    without canonicalisation, so karate is what we use.

use std::collections::BTreeSet;

use noted_index::louvain::{louvain, modularity, Partition};

/// Zachary's karate club: 34 nodes, 78 undirected edges, 0-indexed.
///
/// The standard published edge list (Zachary 1977), as shipped by NetworkX's
/// `karate_club_graph`. Used as the modularity regression benchmark because the
/// Louvain optimum on it is widely reported at Q ≈ 0.4188.
const KARATE_EDGES: [(usize, usize); 78] = [
    (0, 1),
    (0, 2),
    (0, 3),
    (0, 4),
    (0, 5),
    (0, 6),
    (0, 7),
    (0, 8),
    (0, 10),
    (0, 11),
    (0, 12),
    (0, 13),
    (0, 17),
    (0, 19),
    (0, 21),
    (0, 31),
    (1, 2),
    (1, 3),
    (1, 7),
    (1, 13),
    (1, 17),
    (1, 19),
    (1, 21),
    (1, 30),
    (2, 3),
    (2, 7),
    (2, 8),
    (2, 9),
    (2, 13),
    (2, 27),
    (2, 28),
    (2, 32),
    (3, 7),
    (3, 12),
    (3, 13),
    (4, 6),
    (4, 10),
    (5, 6),
    (5, 10),
    (5, 16),
    (6, 16),
    (8, 30),
    (8, 32),
    (8, 33),
    (9, 33),
    (13, 33),
    (14, 32),
    (14, 33),
    (15, 32),
    (15, 33),
    (18, 32),
    (18, 33),
    (19, 33),
    (20, 32),
    (20, 33),
    (22, 32),
    (22, 33),
    (23, 25),
    (23, 27),
    (23, 29),
    (23, 32),
    (23, 33),
    (24, 25),
    (24, 27),
    (24, 31),
    (25, 31),
    (26, 29),
    (26, 33),
    (27, 33),
    (28, 31),
    (28, 33),
    (29, 32),
    (29, 33),
    (30, 32),
    (30, 33),
    (31, 32),
    (31, 33),
    (32, 33),
];

const KARATE_NODES: usize = 34;

fn karate_weighted() -> Vec<(usize, usize, f64)> {
    KARATE_EDGES.iter().map(|&(a, b)| (a, b, 1.0)).collect()
}

/// A node's stable natural key. In production this is `entities.name`
/// (`UNIQUE (workspace_id, name)`), NEVER `entities.id` — see spec §6. Here it
/// stands in for that key so the test exercises the same canonicalisation the
/// caller must perform.
fn node_name(i: usize) -> String {
    format!("n{i:02}")
}

/// What the *caller* must do before handing anything to `louvain`: sort node
/// names, assign indices in that order, rewrite and sort the edge list.
///
/// This is deliberately part of the test rather than of the library, because
/// the library's signature (`node_count` + `&[(usize, usize, f64)]`) exists
/// precisely to force the caller to have done it.
fn canonicalise(
    names: &[String],
    edges: &[(String, String, f64)],
) -> (Vec<String>, Vec<(usize, usize, f64)>) {
    let mut sorted: Vec<String> = names.to_vec();
    sorted.sort();
    let index = |n: &str| sorted.binary_search(&n.to_string()).expect("known node");

    let mut out: Vec<(usize, usize, f64)> = edges
        .iter()
        .map(|(a, b, w)| {
            let (ia, ib) = (index(a), index(b));
            (ia.min(ib), ia.max(ib), *w)
        })
        .collect();
    out.sort_by_key(|x| (x.0, x.1));
    (sorted, out)
}

/// Partition as sets of *names*, for the PERMUTATION tests specifically.
///
/// The re-expression is about node IDENTITY, not about ordering. Those tests
/// permute the input, so node index 3 in one run and node index 3 in another are
/// different graph vertices; only the names carry identity across runs, and no
/// amount of canonicalisation inside [`Partition`] could recover it.
///
/// Ordering is a separate concern and [`Partition`] already owns it: it sorts
/// its communities at construction, so `==` on two `Partition`s over the SAME
/// index space is set-partition equality — see
/// `partition_equality_ignores_the_labels_that_induced_it`. The `BTreeSet`s here
/// are therefore belt-and-braces, not the thing that makes comparison work; an
/// earlier version of this comment claimed they were "the only representation
/// comparable across runs", which contradicted the type's own guarantee.
fn named_partition(part: &Partition, names: &[String]) -> BTreeSet<BTreeSet<String>> {
    part.communities()
        .iter()
        .map(|c| c.iter().map(|&i| names[i].clone()).collect())
        .collect()
}

#[test]
fn karate_club_modularity_matches_reference() {
    let edges = karate_weighted();
    let part = louvain(KARATE_NODES, &edges);
    let q = modularity(KARATE_NODES, &edges, &part);

    // Reference implementation (python-louvain / NetworkX) scores 0.418803 on
    // this graph against the published Louvain optimum of ~0.4188.
    assert!(
        (q - 0.4188).abs() < 0.002,
        "karate modularity was {q}, expected ~0.4188 ({} communities: {:?})",
        part.len(),
        part.communities()
    );
    assert_eq!(
        part.len(),
        4,
        "expected 4 communities on karate, got {}: {:?}",
        part.len(),
        part.communities()
    );

    // Every node is in exactly one community, and nothing is lost.
    let total: usize = part.communities().iter().map(|c| c.len()).sum();
    assert_eq!(total, KARATE_NODES);
}

#[test]
fn modularity_of_the_trivial_partitions_is_known() {
    let edges = karate_weighted();

    // All nodes in one community: Q = 1 - 1 = 0.
    let one = Partition::from_labels(&vec![0; KARATE_NODES]);
    assert!(modularity(KARATE_NODES, &edges, &one).abs() < 1e-12);

    // Every node its own singleton: Q = -sum_i (k_i/2m)^2, strictly negative.
    let singletons = Partition::from_labels(&(0..KARATE_NODES).collect::<Vec<_>>());
    let q = modularity(KARATE_NODES, &edges, &singletons);
    assert!(q < 0.0, "singleton modularity should be negative, was {q}");
}

#[test]
fn twenty_runs_on_identical_input_yield_one_partition() {
    let names: Vec<String> = (0..KARATE_NODES).map(node_name).collect();
    let edges = karate_weighted();

    let distinct: BTreeSet<BTreeSet<BTreeSet<String>>> = (0..20)
        .map(|_| named_partition(&louvain(KARATE_NODES, &edges), &names))
        .collect();

    assert_eq!(
        distinct.len(),
        1,
        "20 runs on identical input produced {} distinct partitions",
        distinct.len()
    );
}

#[test]
fn twenty_runs_on_permuted_input_canonicalised_yield_the_baseline_partition() {
    let names: Vec<String> = (0..KARATE_NODES).map(node_name).collect();
    let name_edges: Vec<(String, String, f64)> = KARATE_EDGES
        .iter()
        .map(|&(a, b)| (node_name(a), node_name(b), 1.0))
        .collect();

    let (base_names, base_edges) = canonicalise(&names, &name_edges);
    let baseline = named_partition(&louvain(base_names.len(), &base_edges), &base_names);

    let mut distinct = BTreeSet::new();
    distinct.insert(baseline.clone());

    for run in 0..20 {
        // A deterministic but structurally different permutation per run: a
        // multiplicative shuffle by a stride coprime with 34, plus an offset.
        // No RNG in the test either — a failure must be reproducible.
        let stride = [3usize, 5, 7, 9, 11, 13, 15, 19, 21, 23][run % 10];
        let offset = run;
        let perm: Vec<usize> = (0..KARATE_NODES)
            .map(|i| (i * stride + offset) % KARATE_NODES)
            .collect();
        assert_eq!(
            perm.iter().copied().collect::<BTreeSet<_>>().len(),
            KARATE_NODES,
            "run {run}: stride {stride} is not coprime with {KARATE_NODES}"
        );

        // Present the same logical graph in a different node order AND a
        // different edge order.
        let mut permuted_names: Vec<String> = perm.iter().map(|&i| node_name(i)).collect();
        permuted_names.reverse();
        let mut permuted_edges: Vec<(String, String, f64)> = name_edges.clone();
        permuted_edges.reverse();
        let rotate = run % permuted_edges.len();
        permuted_edges.rotate_left(rotate);

        let (cnames, cedges) = canonicalise(&permuted_names, &permuted_edges);
        let part = named_partition(&louvain(cnames.len(), &cedges), &cnames);
        distinct.insert(part);
    }

    assert_eq!(
        distinct.len(),
        1,
        "20 permuted runs + baseline produced {} distinct partitions",
        distinct.len()
    );
}

#[test]
fn degenerate_graphs_do_not_panic() {
    // No nodes.
    assert_eq!(louvain(0, &[]).len(), 0);

    // Nodes, no edges: every node is its own community, Q = 0.
    let isolated = louvain(5, &[]);
    assert_eq!(isolated.len(), 5);
    assert!(modularity(5, &[], &isolated).abs() < 1e-12);

    // A single edge: one community of two.
    let one_edge = louvain(2, &[(0, 1, 1.0)]);
    assert_eq!(one_edge.communities(), &[vec![0, 1]]);
}

#[test]
fn two_disjoint_cliques_are_two_communities() {
    let mut edges = Vec::new();
    for a in 0..4 {
        for b in (a + 1)..4 {
            edges.push((a, b, 1.0));
            edges.push((a + 4, b + 4, 1.0));
        }
    }
    let part = louvain(8, &edges);
    assert_eq!(part.communities(), &[vec![0, 1, 2, 3], vec![4, 5, 6, 7]]);
}

/// A graph engineered so that **exact** floating-point gain ties occur.
///
/// Two 4-cliques, plus a bridge node attached to one node of each. By symmetry
/// the bridge's gain toward each clique is computed from bit-identical operands,
/// so it is an exact tie — which is the ONLY situation in which the BTreeMap
/// candidate ordering and the strictly-greater-plus-epsilon comparison can
/// change the outcome. Karate does not contain such a tie (verified by mutation:
/// swapping the BTreeMap for a HashMap and flipping `>` to `>=` leaves every
/// karate assertion green), so without this fixture two of the three stated
/// determinism mechanisms would be entirely untested.
fn tie_graph() -> (usize, Vec<(usize, usize, f64)>) {
    let mut edges = Vec::new();
    for a in 0..4 {
        for b in (a + 1)..4 {
            edges.push((a, b, 1.0));
            edges.push((a + 4, b + 4, 1.0));
        }
    }
    edges.push((0, 8, 1.0));
    edges.push((4, 8, 1.0));
    (9, edges)
}

#[test]
fn exact_gain_ties_resolve_deterministically() {
    let (n, edges) = tie_graph();
    let names: Vec<String> = (0..n).map(node_name).collect();

    let distinct: BTreeSet<BTreeSet<BTreeSet<String>>> = (0..20)
        .map(|_| named_partition(&louvain(n, &edges), &names))
        .collect();

    assert_eq!(
        distinct.len(),
        1,
        "20 runs over a tie-rich graph produced {} distinct partitions",
        distinct.len()
    );

    // The tie must resolve toward the LOWER-numbered community, i.e. the bridge
    // joins the clique containing node 0. This is what "candidates in ascending
    // id, ties lose to the incumbent" buys: a stated, checkable outcome rather
    // than whichever branch the map happened to yield first.
    let part = louvain(n, &edges);
    assert_eq!(
        part.communities(),
        &[vec![0, 1, 2, 3, 8], vec![4, 5, 6, 7]],
        "tie did not resolve toward the lower community id"
    );
}

/// `Partition` claims that `==` is set-partition equality — that the arbitrary
/// integer labels which induced a grouping cannot be observed through it. That
/// claim rests entirely on the `communities.sort()` in `from_labels`, and until
/// this test, deleting that line survived the whole suite: no caller and no
/// other test ever compared two `Partition`s directly.
///
/// The label sets below are chosen so the mechanism is REACHED. `from_labels`
/// groups through a `BTreeMap`, so communities come out ordered by LABEL value;
/// `{0, 1}` yields them in the order `[[0,1],[2,3]]` and `{5, 1}` in the
/// opposite order `[[2,3],[0,1]]`. Same grouping, different emission order —
/// which is exactly the difference the sort exists to erase, and which labels
/// whose relative order happens to agree could never expose.
#[test]
fn partition_equality_ignores_the_labels_that_induced_it() {
    let a = Partition::from_labels(&[0, 0, 1, 1]);
    let b = Partition::from_labels(&[5, 5, 1, 1]);

    assert_eq!(
        a,
        b,
        "two label vectors inducing the same grouping must be the same Partition; labels are \
         arbitrary and must not be observable through ==\n  a: {:?}\n  b: {:?}",
        a.communities(),
        b.communities()
    );
    assert_eq!(
        b.communities(),
        &[vec![0, 1], vec![2, 3]],
        "and the canonical form is specifically the member-sorted one, not whichever order the \
         labels happened to emit"
    );
}
