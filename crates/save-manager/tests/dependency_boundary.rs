//! ENG-DEP-BOUNDARY-01: minimal automated dependency-boundary check.
//!
//! `Project Architecture v1.0` §4.2 forbids `kernel-core` depending on any
//! platform crate, §4.3 forbids workspace dependency cycles, and §4.4 requires
//! the boundary to be enforced by an automated check rather than a written rule
//! alone. `planning/22` §九 requires this to be in place before or together with
//! the new `save-manager` crate, and `SAVE-001` acceptance must not assume the
//! check already exists.
//!
//! The workspace graph is read from `cargo metadata`; the checker itself is a
//! pure function, so violation detection is covered by negative tests rather
//! than by injecting a real forbidden dependency.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// crate name -> names of the workspace members it depends on.
type Graph = BTreeMap<String, Vec<String>>;

const KERNEL_CORE: &str = "kernel-core";

/// Platform crates `kernel-core` MUST NOT depend on (Architecture §4.2).
const PLATFORM_CRATES: [&str; 2] = ["platform-core", "save-manager"];

#[derive(Clone, Debug, Eq, PartialEq)]
enum BoundaryViolation {
    ForbiddenEdge { from: String, to: String },
    Cycle { members: Vec<String> },
}

/// Verifies forbidden edges and dependency cycles over a workspace graph.
fn check_boundary(graph: &Graph) -> Result<(), BoundaryViolation> {
    if let Some(edges) = graph.get(KERNEL_CORE) {
        for to in edges {
            if PLATFORM_CRATES.contains(&to.as_str()) {
                return Err(BoundaryViolation::ForbiddenEdge {
                    from: KERNEL_CORE.to_owned(),
                    to: to.clone(),
                });
            }
        }
    }

    let mut state: BTreeMap<&str, u8> = BTreeMap::new();
    let mut stack: Vec<&str> = Vec::new();
    for node in graph.keys() {
        if state.get(node.as_str()).copied().unwrap_or(0) == 0
            && let Some(violation) = visit(node.as_str(), graph, &mut state, &mut stack)
        {
            return Err(violation);
        }
    }
    Ok(())
}

fn visit<'a>(
    node: &'a str,
    graph: &'a Graph,
    state: &mut BTreeMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
) -> Option<BoundaryViolation> {
    state.insert(node, 1);
    stack.push(node);
    if let Some(neighbours) = graph.get(node) {
        for next in neighbours {
            if !graph.contains_key(next.as_str()) {
                continue;
            }
            match state.get(next.as_str()).copied().unwrap_or(0) {
                0 => {
                    if let Some(violation) = visit(next.as_str(), graph, state, stack) {
                        return Some(violation);
                    }
                }
                1 => {
                    let start = stack
                        .iter()
                        .position(|entry| *entry == next.as_str())
                        .unwrap_or(0);
                    return Some(BoundaryViolation::Cycle {
                        members: stack[start..]
                            .iter()
                            .map(|entry| (*entry).to_owned())
                            .collect(),
                    });
                }
                _ => {}
            }
        }
    }
    stack.pop();
    state.insert(node, 2);
    None
}

/// Reads the real workspace dependency graph via `cargo metadata --no-deps`.
fn workspace_graph() -> Graph {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under the workspace root");
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata is JSON");
    let packages = metadata["packages"]
        .as_array()
        .expect("cargo metadata packages");

    let members: BTreeSet<String> = packages
        .iter()
        .map(|package| package["name"].as_str().expect("package name").to_owned())
        .collect();

    let mut graph = Graph::new();
    for package in packages {
        let name = package["name"].as_str().expect("package name").to_owned();
        let mut edges: Vec<String> = package["dependencies"]
            .as_array()
            .expect("package dependencies")
            .iter()
            .filter_map(|dependency| dependency["name"].as_str())
            .filter(|dependency| members.contains(*dependency))
            .map(str::to_owned)
            .collect();
        edges.sort();
        edges.dedup();
        graph.insert(name, edges);
    }
    graph
}

#[test]
fn workspace_respects_dependency_boundaries() {
    let graph = workspace_graph();
    assert!(
        graph.contains_key(KERNEL_CORE),
        "kernel-core must be a workspace member"
    );
    assert!(
        graph.contains_key("save-manager"),
        "save-manager must be a workspace member"
    );
    assert_eq!(
        check_boundary(&graph),
        Ok(()),
        "the workspace must have no forbidden edge and no dependency cycle"
    );
    assert!(
        !graph[KERNEL_CORE]
            .iter()
            .any(|dependency| PLATFORM_CRATES.contains(&dependency.as_str())),
        "kernel-core must not depend on a platform crate"
    );
}

#[test]
fn checker_rejects_a_forbidden_kernel_to_platform_edge() {
    let graph: Graph = BTreeMap::from([
        (KERNEL_CORE.to_owned(), vec!["save-manager".to_owned()]),
        ("save-manager".to_owned(), Vec::new()),
    ]);
    assert_eq!(
        check_boundary(&graph),
        Err(BoundaryViolation::ForbiddenEdge {
            from: KERNEL_CORE.to_owned(),
            to: "save-manager".to_owned(),
        })
    );
}

#[test]
fn checker_rejects_a_dependency_cycle() {
    let graph: Graph = BTreeMap::from([
        ("kernel-core".to_owned(), Vec::new()),
        ("platform-core".to_owned(), vec!["save-manager".to_owned()]),
        ("save-manager".to_owned(), vec!["platform-core".to_owned()]),
    ]);
    match check_boundary(&graph) {
        Err(BoundaryViolation::Cycle { members }) => {
            assert!(members.contains(&"platform-core".to_owned()));
            assert!(members.contains(&"save-manager".to_owned()));
        }
        other => panic!("expected a cycle violation, got {other:?}"),
    }
}
