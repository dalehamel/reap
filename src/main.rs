#[macro_use]
extern crate clap;
#[macro_use]
extern crate serde;
extern crate petgraph;
extern crate serde_json;

use petgraph::algo::dominators;
use petgraph::dot;
use petgraph::graph::NodeIndex;
use petgraph::{Directed, Graph};
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::prelude::*;
use std::io::BufReader;

#[derive(Debug, Deserialize)]
struct Line {
    address: Option<String>,
    memsize: Option<usize>,

    #[serde(default)]
    references: Vec<String>,

    #[serde(rename = "type")]
    object_type: String,

    class: Option<String>,
    root: Option<String>,
    name: Option<String>,
    length: Option<usize>,
    size: Option<usize>,
    value: Option<String>,
}

#[derive(Debug)]
struct ParsedLine {
    object: Object,
    references: Vec<usize>,
    module: Option<usize>,
    name: Option<String>,
}

#[derive(Debug, Clone)]
struct Object {
    address: usize,
    bytes: usize,
    kind: String,
    label: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Stats {
    count: usize,
    bytes: usize,
}

const DEFAULT_RELEVANCE_THRESHOLD: f64 = 0.005;

impl Line {
    pub fn parse(self) -> Option<ParsedLine> {
        let mut object = Object {
            address: self
                .address
                .as_ref()
                .map(|a| Line::parse_address(a.as_str()))
                .unwrap_or(0),
            bytes: self.memsize.unwrap_or(0),
            kind: self.object_type,
            label: None,
        };

        if object.address == 0 && object.kind != "ROOT" {
            return None;
        }

        object.label = match object.kind.as_str() {
            "CLASS" | "MODULE" | "ICLASS" => {
                self.name.clone().map(|n| format!("{}[{}]", n, object.kind))
            }
            "ARRAY" => Some(format!("Array[len={}]", self.length?)),
            "HASH" => Some(format!("Hash[size={}]", self.size?)),
            "STRING" => self.value.map(|v| {
                v.chars()
                    .take(40)
                    .flat_map(|c| {
                        // Hacky escape to prevent dot format from breaking
                        if c.is_control() {
                            None
                        } else if c == '\\' {
                            Some('﹨')
                        } else {
                            Some(c)
                        }
                    })
                    .collect::<String>()
            }),
            _ => None,
        };

        Some(ParsedLine {
            references: self
                .references
                .iter()
                .map(|r| Line::parse_address(r.as_str()))
                .collect(),
            module: self.class.map(|c| Line::parse_address(c.as_str())),
            name: self.name,
            object,
        })
    }

    fn parse_address(addr: &str) -> usize {
        usize::from_str_radix(&addr[2..], 16).unwrap()
    }
}

impl Object {
    pub fn stats(&self) -> Stats {
        Stats {
            count: 1,
            bytes: self.bytes,
        }
    }

    pub fn root() -> Object {
        Object {
            address: 0,
            bytes: 0,
            kind: "ROOT".to_string(),
            label: Some("root".to_string()),
        }
    }

    pub fn is_root(&self) -> bool {
        self.address == 0
    }
}

impl PartialEq for Object {
    fn eq(&self, other: &Object) -> bool {
        self.address == other.address
    }
}
impl Eq for Object {}

impl Hash for Object {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

impl Display for Object {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if let Some(ref label) = self.label {
            write!(f, "{}", label)
        } else {
            write!(f, "{}[{}]", self.kind, self.address)
        }
    }
}

impl Stats {
    pub fn add(&mut self, other: Stats) -> Stats {
        Stats {
            count: self.count + other.count,
            bytes: self.bytes + other.bytes,
        }
    }
}

type ReferenceGraph = Graph<Object, &'static str, Directed, usize>;

fn parse(file: &str) -> std::io::Result<(NodeIndex<usize>, ReferenceGraph)> {
    let file = File::open(file)?;
    let reader = BufReader::new(file);

    let mut graph: ReferenceGraph = Graph::default();
    let mut indices: HashMap<usize, NodeIndex<usize>> = HashMap::new();
    let mut references: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut instances: HashMap<usize, usize> = HashMap::new();
    let mut names: HashMap<usize, String> = HashMap::new();

    let root = Object::root();
    let root_address = root.address;
    let root_index = graph.add_node(root);
    indices.insert(root_address, root_index);
    references.insert(root_address, Vec::new());

    for line in reader.lines().map(|l| l.unwrap()) {
        let parsed = serde_json::from_str::<Line>(&line)
            .expect(&line)
            .parse()
            .expect(&line);

        if parsed.object.is_root() {
            let refs = references.get_mut(&root_address).unwrap();
            refs.extend_from_slice(parsed.references.as_slice());
        } else {
            let address = parsed.object.address;
            indices.insert(address, graph.add_node(parsed.object));

            if !parsed.references.is_empty() {
                references.insert(address, parsed.references);
            }
            if let Some(module) = parsed.module {
                instances.insert(address, module);
            }
            if let Some(name) = parsed.name {
                names.insert(address, name);
            }
        }
    }

    for (node, successors) in references {
        let i = &indices[&node];
        for s in successors {
            if let Some(j) = indices.get(&s) {
                graph.add_edge(*i, *j, "");
            }
        }
    }

    for mut obj in graph.node_weights_mut() {
        if let Some(module) = instances.get(&obj.address) {
            if let Some(name) = names.get(module) {
                obj.kind = name.to_owned();
            }
        }
    }

    Ok((root_index, graph))
}

fn stats_by_kind(graph: &ReferenceGraph) -> HashMap<&str, Stats> {
    let mut by_kind: HashMap<&str, Stats> = HashMap::new();
    for i in graph.node_indices() {
        let obj = graph.node_weight(i).unwrap();
        by_kind
            .entry(&obj.kind)
            .and_modify(|c| *c = (*c).add(obj.stats()))
            .or_insert_with(|| obj.stats());
    }
    by_kind
}

fn dominator_subtree_sizes(
    root: NodeIndex<usize>,
    graph: &ReferenceGraph,
) -> HashMap<&Object, Stats> {
    let tree = dominators::simple_fast(graph, root);

    let mut subtree_sizes: HashMap<&Object, Stats> = HashMap::new();

    // Assign each node's stats to itself
    for i in graph.node_indices() {
        let obj = graph.node_weight(i).unwrap();
        subtree_sizes.insert(obj, obj.stats());
    }

    // Assign each node's stats to all of its dominators
    for mut i in graph.node_indices() {
        let obj = graph.node_weight(i).unwrap();
        let stats = obj.stats();

        while let Some(dom) = tree.immediate_dominator(i) {
            i = dom;

            subtree_sizes
                .entry(graph.node_weight(i).unwrap())
                .and_modify(|e| *e = (*e).add(stats));
        }
    }

    subtree_sizes
}

fn relevant_subgraph<'a>(
    root: NodeIndex<usize>,
    graph: &'a ReferenceGraph,
    subtree_sizes: &HashMap<&'a Object, Stats>,
    relevance_threshold: f64,
) -> ReferenceGraph {
    let mut subgraph: ReferenceGraph = graph.clone();

    let threshold_bytes = (subtree_sizes[graph.node_weight(root).unwrap()].bytes as f64
        * relevance_threshold)
        .floor() as usize;

    subgraph.retain_nodes(|g, n| {
        let obj = g.node_weight(n).unwrap();
        subtree_sizes[obj].bytes >= threshold_bytes
    });

    // It's not clear to me why removing nodes per above leaves us with duplicate edges
    let mut seen: HashSet<(NodeIndex<usize>, NodeIndex<usize>)> = HashSet::new();
    subgraph.retain_edges(|g, e| {
        let (v, w) = g.edge_endpoints(e).unwrap();
        v != w && seen.insert((v, w))
    });

    for mut obj in subgraph.node_weights_mut() {
        let Stats { count, bytes } = subtree_sizes[obj];
        obj.label = Some(format!(
            "{}: {}b self, {}b refs, {} objects",
            obj,
            obj.bytes,
            bytes - obj.bytes,
            count
        ));
    }

    subgraph
}

fn write_dot_file(graph: &ReferenceGraph, filename: &str) -> std::io::Result<()> {
    let mut file = File::create(filename)?;
    write!(
        file,
        "{}",
        dot::Dot::with_config(&graph, &[dot::Config::EdgeNoLabel])
    )?;
    Ok(())
}

fn print_largest<K: Display + Eq + Hash>(map: &HashMap<K, Stats>, count: usize) {
    let sorted = {
        let mut vec: Vec<(&K, &Stats)> = map.iter().collect();
        vec.sort_unstable_by_key(|(_, c)| c.bytes);
        vec
    };
    for (k, stats) in sorted.iter().rev().take(count) {
        println!("{}: {} bytes ({} objects)", k, stats.bytes, stats.count);
    }
    let rest = sorted
        .iter()
        .rev()
        .skip(count)
        .fold(Stats::default(), |mut acc, (_, c)| acc.add(**c));
    println!("...: {} bytes ({} objects)", rest.bytes, rest.count);
}

fn main() -> std::io::Result<()> {
    let args = clap_app!(reap =>
        (version: "0.1")
        (about: "A tool for parsing Ruby heap dumps.")
        (@arg INPUT: +required "Path to JSON heap dump file")
        (@arg DOT: -d --dot +takes_value "Dot file output")
        (@arg THRESHOLD: -t --threshold +takes_value "Include nodes retaining at least this fraction of memory in dot output (defaults to 0.005)")
    )
    .get_matches();

    let input = args.value_of("INPUT").unwrap();
    let (root, graph) = parse(&input)?;
    let by_kind = stats_by_kind(&graph);
    println!("Object types using the most memory:");
    print_largest(&by_kind, 10);

    let subtree_sizes = dominator_subtree_sizes(root, &graph);
    println!("\nObjects retaining the most memory:");
    print_largest(&subtree_sizes, 25);

    if let Some(output) = args.value_of("DOT") {
        let threshold: f64 = args
            .value_of("THRESHOLD")
            .map(|t| t.parse().unwrap())
            .unwrap_or(DEFAULT_RELEVANCE_THRESHOLD);
        let dom_graph = relevant_subgraph(root, &graph, &subtree_sizes, threshold);
        write_dot_file(&dom_graph, &output)?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn integration() {
        let (root, graph) = parse("test/heap.json").unwrap();

        assert_eq!(18982, graph.node_count());
        assert_eq!(28436, graph.edge_count());

        let by_kind = stats_by_kind(&graph);
        assert_eq!(10409, by_kind["String"].count);
        assert_eq!(544382, by_kind["String"].bytes);

        let subtree_sizes = dominator_subtree_sizes(root, &graph);
        let root_obj = graph.node_weight(root).unwrap();
        assert_eq!(15472, subtree_sizes[root_obj].count);
        assert_eq!(3439119, subtree_sizes[root_obj].bytes);

        let dom_graph = relevant_subgraph(root, &graph, &subtree_sizes, DEFAULT_RELEVANCE_THRESHOLD);
        assert_eq!(33, dom_graph.node_count());
        assert_eq!(37, dom_graph.edge_count());
    }
}
