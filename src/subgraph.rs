//! A subgraph in a GBZ graph.
//!
//! This module provides functionality for extracting a subgraph around a specific position or interval of a specific path.
//! The subgraph contains all nodes within a given context and all edges between them.
//! All paths within the subgraph are also extracted, but they do not have any metadata associated with them.

// TODO: tests

use crate::{GBZRecord, GBZPath, GraphInterface, FullPathName};
use crate::formats::{self, WalkMetadata, JSONValue};

use std::cmp::Reverse;
use std::collections::{BinaryHeap, BTreeMap};
use std::fmt::Display;
use std::io::{self, Write};
use std::ops::Range;

use gbwt::{Orientation, Pos, ENDMARKER};

use gbwt::support;

//-----------------------------------------------------------------------------

/// Output options for the haplotypes in the subgraph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HaplotypeOutput {
    /// Output all haplotypes as separate paths.
    All,

    /// Output only distinct haplotypes with the number of duplicates stored in the weight field.
    Distinct,

    /// Output only the reference path.
    ReferenceOnly,
}

// TODO: QueryArguments: path name, offset, context, output?

//-----------------------------------------------------------------------------

// TODO: PathIndex; build it from GBZ using reference_positions()

//-----------------------------------------------------------------------------

/// A subgraph based on a context around a position or an interval of a specific path.
///
/// The path used for extracting the subgraph becomes the reference path for it.
/// Non-reference haplotypes do not have any metadata associated with them, as we cannot determine the identifier of a path from its GBWT position efficiently.
pub struct Subgraph {
    // Node records for the subgraph.
    records: BTreeMap<usize, GBZRecord>,

    // Paths in the subgraph.
    paths: Vec<PathInfo>,

    // Offset in `paths` for the reference path.
    ref_id: usize,

    // Metadata for the reference path.
    ref_path: GBZPath,

    // Interval of the reference path that is present in the subgraph.
    ref_interval: Range<usize>,
}

impl Subgraph {
    // TODO: From GBZ + PathIndex

    /// Extracts a subgraph around a specific position in a specific path.
    ///
    /// # Arguments
    ///
    /// * `graph`: Graph interface for a GBZ-base graph.
    /// * `path_name`: Name of the path to use as the reference path.
    /// * `offset`: Position in the reference path (in bp).
    /// * `context`: Context size around the reference position (in bp).
    /// * `haplotype_output`: How to output the haplotypes.
    pub fn new(graph: &mut GraphInterface, path_name: &FullPathName, offset: usize, context: usize, haplotype_output: HaplotypeOutput) -> Result<Self, String> {
        // Find the reference path.
        let ref_path = graph.find_path(path_name)?;
        let ref_path = ref_path.ok_or(format!("Cannot find path {}", path_name))?;
        if !ref_path.is_indexed {
            return Err(format!("Path {} has not been indexed for random access", path_name));
        }

        let (query_pos, gbwt_pos) = query_position(graph, &ref_path, offset)?;
        let records = extract_context(graph, query_pos, context)?;
        let (mut paths, (mut ref_id, path_offset)) = extract_paths(&records, gbwt_pos)?;
        let ref_start = offset - distance_to(&records, &paths[ref_id].path, path_offset, query_pos.offset);
        let ref_interval = ref_start..ref_start + paths[ref_id].len;

        if haplotype_output == HaplotypeOutput::Distinct {
            // TODO: We should use bidirectional search in GBWT to find the distinct paths directly.
            (paths, ref_id) = make_distinct(paths, ref_id);
        } else if haplotype_output == HaplotypeOutput::ReferenceOnly {
            paths = vec![paths[ref_id].clone()];
            ref_id = 0;
        }

        Ok(Subgraph { records, paths, ref_id, ref_path, ref_interval, })
    }

    // Returns the total length of the nodes in the given path interval.
    fn path_len(&self, path: &[usize], interval: Range<usize>) -> usize {
        let mut result = 0;
        for i in interval {
            let record = self.records.get(&path[i]).unwrap();
            result += record.sequence_len();
        }
        result
    }

    // Appends a new edit or extends the previous one.
    fn append_edit(edits: &mut Vec<(EditOperation, usize)>, op: EditOperation, len: usize) {
        if let Some((prev_op, prev_len)) = edits.last_mut() {
            if *prev_op == op {
                *prev_len += len;
                return;
            }
        }
        edits.push((op, len));
    }

    // Appends the relevant edits for two path intervals, which are assumed to be diverging.
    fn append_edits(&self, edits: &mut Vec<(EditOperation, usize)>, path: &[usize], path_interval: Range<usize>, ref_path: &[usize], ref_interval: Range<usize>) {
        let path_len = self.path_len(path, path_interval);
        let ref_len = self.path_len(ref_path, ref_interval);
        if path_len == ref_len && path_len > 0 && path_len < 5 {
            Self::append_edit(edits, EditOperation::Match, path_len);
        } else {
            if path_len > 0 {
                Self::append_edit(edits, EditOperation::Insertion, path_len);
            }
            if ref_len > 0 {
                Self::append_edit(edits, EditOperation::Deletion, ref_len);
            }
        }
    }

    // Returns the CIGAR string for the given path, aligned to the reference path.
    // Takes the alignment from the longest common subsequence of the node sequences.
    // Diverging parts that are of equal length and at most 4 bp are represented as
    // matches. Otherwise they are represented as an insertion and/or a deletion.
    fn align_to_ref(&self, path_id: usize) -> Option<String> {
        if path_id == self.ref_id || path_id >= self.paths.len() {
            return None;
        }

        // Fill in the LCS matrix.
        // TODO: Something more efficient?
        let ref_path = &self.paths[self.ref_id].path;
        let path = &self.paths[path_id].path;
        let mut dp_matrix = vec![vec![0; ref_path.len() + 1]; path.len() + 1];
        for path_offset in 0..path.len() {
            for ref_offset in 0..ref_path.len() {
                if path[path_offset] == ref_path[ref_offset] {
                    dp_matrix[path_offset + 1][ref_offset + 1] = dp_matrix[path_offset][ref_offset] + 1;
                } else {
                    dp_matrix[path_offset + 1][ref_offset + 1] = dp_matrix[path_offset][ref_offset + 1].max(dp_matrix[path_offset + 1][ref_offset]);
                }
            }
        }

        // Trace back the LCS.
        let mut lcs: Vec<(usize, usize)> = Vec::new();
        let mut path_offset = path.len();
        let mut ref_offset = ref_path.len();
        while path_offset > 0 && ref_offset > 0 {
            if path[path_offset - 1] == ref_path[ref_offset - 1] {
                lcs.push((path_offset - 1, ref_offset - 1));
                path_offset -= 1;
                ref_offset -= 1;
            } else if dp_matrix[path_offset - 1][ref_offset] > dp_matrix[path_offset][ref_offset - 1] {
                path_offset -= 1;
            } else {
                ref_offset -= 1;
            }
        }
        lcs.reverse();

        // Convert the LCS to a sequence of edit operations
        let mut edits: Vec<(EditOperation, usize)> = Vec::new();
        path_offset = 0;
        ref_offset = 0;
        for (next_path_offset, next_ref_offset) in lcs.iter() {
            self.append_edits(&mut edits, path, path_offset..*next_path_offset, ref_path, ref_offset..*next_ref_offset);
            let node_len = self.records.get(&path[*next_path_offset]).unwrap().sequence_len();
            Self::append_edit(&mut edits, EditOperation::Match, node_len);
            path_offset = next_path_offset + 1;
            ref_offset = next_ref_offset + 1;
        }
        self.append_edits(&mut edits, path, path_offset..path.len(), ref_path, ref_offset..ref_path.len());

        // Convert the edits to a CIGAR string.
        let mut result = String::new();
        for (op, len) in edits.iter() {
            result.push_str(&format!("{}{}", len, op));
        }
        Some(result)
    }

    /// Writes the subgraph in the GFA format to the given output.
    ///
    /// If `cigar` is true, the CIGAR strings for the non-reference haplotypes are included in the output.
    pub fn write_gfa<T: Write>(&self, output: &mut T, cigar: bool) -> io::Result<()> {
        // Header.
        let reference_samples = Some(self.ref_path.name.sample.clone());
        formats::write_gfa_header(reference_samples, output)?;

        // Segments.
        for (handle, record) in self.records.iter() {
            if support::node_orientation(*handle) == Orientation::Forward {
                formats::write_gfa_segment(record, output)?;
            }
        }

        // Links.
        for (handle, record) in self.records.iter() {
            let from = support::decode_node(*handle);
            for successor in record.successors() {
                let to = support::decode_node(successor);
                if self.records.contains_key(&successor) && support::edge_is_canonical(from, to) {
                    formats::write_gfa_link(
                        (from.0.to_string().as_bytes(), from.1),
                        (to.0.to_string().as_bytes(), to.1),
                        output
                    )?;
                }
            }
        }

        // Paths.
        let ref_metadata = WalkMetadata::path_interval(
            &self.ref_path,
            self.ref_interval.clone(),
            self.paths[self.ref_id].weight
        );
        formats::write_gfa_walk(&self.paths[self.ref_id].path, &ref_metadata, output)?;
        if self.paths.len() > 1 {
            let mut haplotype = 1;
            for (id, path_info) in self.paths.iter().enumerate() {
                if id == self.ref_id {
                    continue;
                }
                let mut metadata = WalkMetadata::anonymous(
                    haplotype,
                    &self.ref_path.name.contig,
                    path_info.len,
                    path_info.weight
                );
                if cigar {
                    metadata.add_cigar(self.align_to_ref(id));
                }
                formats::write_gfa_walk(&path_info.path, &metadata, output)?;
                haplotype += 1;
            }
        }

        Ok(())
    }

    /// Writes the subgraph in the JSON format to the given output.
    ///
    /// If `cigar` is true, the CIGAR strings for the non-reference haplotypes are included in the output.
    pub fn write_json<T: Write>(&self, output: &mut T, cigar: bool) -> io::Result<()> {
        // Nodes.
        let mut nodes: Vec<JSONValue> = Vec::new();
        for (_, record) in self.records.iter() {
            let (id, orientation) = support::decode_node(record.handle());
            if orientation == Orientation::Reverse {
                continue;
            }
            let node = JSONValue::Object(vec![
                ("id".to_string(), JSONValue::String(id.to_string())),
                ("sequence".to_string(), JSONValue::String(String::from_utf8_lossy(record.sequence()).to_string())),
            ]);
            nodes.push(node);
        }

        // Edges.
        let mut edges: Vec<JSONValue> = Vec::new();
        for (handle, record) in self.records.iter() {
            let from = support::decode_node(*handle);
            for successor in record.successors() {
                let to = support::decode_node(successor);
                if self.records.contains_key(&successor) && support::edge_is_canonical(from, to) {
                    let edge = JSONValue::Object(vec![
                        ("from".to_string(), JSONValue::String(from.0.to_string())),
                        ("from_is_reverse".to_string(), JSONValue::Boolean(from.1 == Orientation::Reverse)),
                        ("to".to_string(), JSONValue::String(to.0.to_string())),
                        ("to_is_reverse".to_string(), JSONValue::Boolean(to.1 == Orientation::Reverse)),
                    ]);
                    edges.push(edge);
                }
            }
        }

        // Paths.
        let mut paths: Vec<JSONValue> = Vec::new();
        let ref_metadata = WalkMetadata::path_interval(
            &self.ref_path,
            self.ref_interval.clone(),
            self.paths[self.ref_id].weight
        );
        let ref_path = formats::json_path(&self.paths[self.ref_id].path, &ref_metadata);
        paths.push(ref_path);
        let mut haplotype = 1;
        for (id, path_info) in self.paths.iter().enumerate() {
            if id == self.ref_id {
                continue;
            }
            let mut metadata = WalkMetadata::anonymous(
                haplotype,
                &self.ref_path.name.contig,
                path_info.len,
                path_info.weight
            );
            if cigar {
                metadata.add_cigar(self.align_to_ref(id));
            }
            let path = formats::json_path(&path_info.path, &metadata);
            paths.push(path);
            haplotype += 1;
        }

        let result = JSONValue::Object(vec![
            ("nodes".to_string(), JSONValue::Array(nodes)),
            ("edges".to_string(), JSONValue::Array(edges)),
            ("paths".to_string(), JSONValue::Array(paths)),
        ]);
        output.write_all(result.to_string().as_bytes())?;

        Ok(())
    }
}

//-----------------------------------------------------------------------------

// Returns the graph position and the GBWT position for the given offset.
fn query_position(graph: &mut GraphInterface, path: &GBZPath, query_offset: usize) -> Result<(GraphPosition, Pos), String> {
    let result = graph.indexed_position(path.handle, query_offset)?;
    let (mut path_offset, mut pos) = result.ok_or(format!("Path {} is not indexed", path.name()))?;

    let mut graph_pos: Option<GraphPosition> = None;
    let mut gbwt_pos: Option<Pos> = None;
    loop {
        let handle = pos.node;
        let record = graph.get_record(handle)?;
        let record = record.ok_or(format!("The graph does not contain handle {}", handle))?;
        if path_offset + record.sequence_len() > query_offset {
            graph_pos = Some(GraphPosition {
                node: support::node_id(handle),
                orientation: support::node_orientation(handle),
                offset: query_offset - path_offset,
            });
            gbwt_pos = Some(pos);
            break;
        }
        path_offset += record.sequence_len();
        let gbwt_record = record.to_gbwt_record();
        let next = gbwt_record.lf(pos.offset);
        if next.is_none() {
            break;
        }
        pos = next.unwrap();
    }

    let graph_pos = graph_pos.ok_or(
        format!("Path {} does not contain offset {}", path.name(), query_offset)
    )?;
    let gbwt_pos = gbwt_pos.unwrap();
    Ok((graph_pos, gbwt_pos))
}

//-----------------------------------------------------------------------------

fn distance_to_end(record: &GBZRecord, orientation: Orientation, offset: usize) -> usize {
    if orientation == support::node_orientation(record.handle()) {
        record.sequence_len() - offset
    } else {
        offset + 1
    }
}

fn extract_context(
    graph: &mut GraphInterface,
    from: GraphPosition,
    context: usize
) -> Result<BTreeMap<usize, GBZRecord>, String> {
    // Start graph traversal from the initial node.
    let mut active: BinaryHeap<Reverse<(usize, usize)>> = BinaryHeap::new(); // (distance, node id)
    active.push(Reverse((0, from.node)));

    // Traverse in both directions.
    let mut selected: BTreeMap<usize, GBZRecord> = BTreeMap::new();
    while !active.is_empty() {
        let (distance, node_id) = active.pop().unwrap().0;
        for orientation in [Orientation::Forward, Orientation::Reverse] {
            let handle = support::encode_node(node_id, orientation);
            if selected.contains_key(&handle) {
                continue;
            }
            let record = graph.get_record(handle)?;
            let record = record.ok_or(format!("The graph does not contain handle {}", handle))?;
            let next_distance = if node_id == from.node {
                distance_to_end(&record, from.orientation, from.offset)
            } else {
                distance + record.sequence_len()
            };
            if next_distance <= context {
                for successor in record.successors() {
                    if !selected.contains_key(&successor) {
                        active.push(Reverse((next_distance, support::node_id(successor))));
                    }
                }
            }
            selected.insert(handle, record);
        }
    }

    Ok(selected)
}

//-----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PathInfo {
    path: Vec<usize>,
    len: usize,
    weight: Option<usize>,
}

impl PathInfo {
    fn new(path: Vec<usize>, len: usize) -> Self {
        PathInfo { path, len, weight: None }
    }

    fn weighted(path: Vec<usize>, len: usize) -> Self {
        PathInfo { path, len, weight: Some(1) }
    }

    fn increment_weight(&mut self) {
        if let Some(weight) = self.weight {
            self.weight = Some(weight + 1);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum EditOperation {
    Match,
    Insertion,
    Deletion,
}

impl Display for EditOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditOperation::Match => write!(f, "M"),
            EditOperation::Insertion => write!(f, "I"),
            EditOperation::Deletion => write!(f, "D"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct GraphPosition {
    node: usize,
    orientation: Orientation,
    offset: usize,
}

//-----------------------------------------------------------------------------

fn next_pos(pos: Pos, successors: &BTreeMap<usize, Vec<(Pos, bool)>>) -> Option<Pos> {
    if let Some(v) = successors.get(&pos.node) {
        let (next, _) = v[pos.offset];
        if next.node == ENDMARKER || !successors.contains_key(&next.node) {
            None
        } else {
            Some(next)
        }
    } else {
        None
    }
}

// Extract all paths in the subgraph. The second return value is
// (offset in result, offset on that path) for the handle corresponding to `ref_pos`.
fn extract_paths(
    subgraph: &BTreeMap<usize, GBZRecord>,
    ref_pos: Pos
) -> Result<(Vec<PathInfo>, (usize, usize)), String> {
    // Decompress the GBWT node records for the subgraph.
    let mut keys: Vec<usize> = Vec::new();
    let mut successors: BTreeMap<usize, Vec<(Pos, bool)>> = BTreeMap::new();
    for (handle, gbz_record) in subgraph.iter() {
        let gbwt_record = gbz_record.to_gbwt_record();
        let decompressed: Vec<(Pos, bool)> = gbwt_record.decompress().into_iter().map(|x| (x, false)).collect();
        keys.push(*handle);
        successors.insert(*handle, decompressed);
    }

    // Mark the positions that have predecessors in the subgraph.
    for handle in keys.iter() {
        let decompressed = successors.get(handle).unwrap().clone();
        for (pos, _) in decompressed.iter() {
            if let Some(v) = successors.get_mut(&pos.node) {
                v[pos.offset].1 = true;
            }
        }
    }

    // FIXME: Check for infinite loops.
    // Extract all paths and note if one of them passes through `ref_pos`.
    let mut result: Vec<PathInfo> = Vec::new();
    let mut ref_id_offset: Option<(usize, usize)> = None;
    for (handle, positions) in successors.iter() {
        for (offset, (_, has_predecessor)) in positions.iter().enumerate() {
            if *has_predecessor {
                continue;
            }
            let mut curr = Some(Pos::new(*handle, offset));
            let mut is_ref = false;
            let mut path: Vec<usize> = Vec::new();
            let mut len = 0;
            while let Some(pos) = curr {
                if pos == ref_pos {
                    ref_id_offset = Some((result.len(), path.len()));
                    is_ref = true;
                }
                path.push(pos.node);
                len += subgraph.get(&pos.node).unwrap().sequence_len();
                curr = next_pos(pos, &successors);
            }
            if is_ref {
                if !support::encoded_path_is_canonical(&path) {
                    eprintln!("Warning: the reference path is not in canonical orientation");
                }
                result.push(PathInfo::new(path, len));
            } else if support::encoded_path_is_canonical(&path) {
                result.push(PathInfo::new(path, len));
            }
        }
    }

    let ref_id_offset = ref_id_offset.ok_or("Could not find the reference path".to_string())?;
    Ok((result, ref_id_offset))
}

fn distance_to(subgraph: &BTreeMap<usize, GBZRecord>, path: &[usize], path_offset: usize, node_offset: usize) -> usize {
    let mut result = node_offset;
    for handle in path.iter().take(path_offset) {
        result += subgraph.get(handle).unwrap().sequence_len();
    }
    result
}

// Returns all distinct paths and uses the weight field for storing their counts.
// Also updates `ref_id`.
// TODO: Use hashing?
fn make_distinct(
    paths: Vec<PathInfo>,
    ref_id: usize
) -> (Vec<PathInfo>, usize) {
    let ref_path = paths[ref_id].path.clone();
    let mut paths = paths;
    paths.sort_unstable();

    let mut new_paths: Vec<PathInfo> = Vec::new();
    let mut ref_id = 0;
    for info in paths.iter() {
        if new_paths.is_empty() || new_paths.last().unwrap().path != info.path {
            if info.path == ref_path {
                ref_id = new_paths.len();
            }
            new_paths.push(PathInfo::weighted(info.path.clone(), info.len));
        } else {
            new_paths.last_mut().unwrap().increment_weight();
        }
    }

    (new_paths, ref_id)
}

//-----------------------------------------------------------------------------
