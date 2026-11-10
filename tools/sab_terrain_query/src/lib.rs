use sab_terrain::{geometry::TerrainMesh, retail::ChunkSource};
use std::{cmp::Ordering, path::Path};

const LEAF_SIZE: usize = 8;
type Bounds = [[f32; 3]; 2];

#[derive(Clone, Debug)]
pub struct Source {
    pub location: ChunkSource,
    pub mesh: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TriangleKey {
    pub source: u32,
    pub triangle: u32,
}

#[derive(Clone, Copy, Debug)]
struct Primitive {
    vertices: [[f32; 3]; 3],
    key: TriangleKey,
}

#[derive(Debug)]
enum NodeKind {
    Leaf { start: usize, end: usize },
    Branch { left: usize, right: usize },
}

#[derive(Debug)]
struct Node {
    bounds: Bounds,
    kind: NodeKind,
}

#[derive(Default)]
pub struct Builder {
    primitives: Vec<Primitive>,
    sources: Vec<Source>,
}

impl Builder {
    pub fn add(&mut self, source: &ChunkSource, mesh: &TerrainMesh) -> Result<(), String> {
        let source_id = u32::try_from(self.sources.len()).map_err(|_| "too many sources")?;
        if mesh.triangles.len() > u32::MAX as usize
            || mesh
                .vertices
                .iter()
                .flatten()
                .any(|value| !value.is_finite())
            || mesh
                .triangles
                .iter()
                .flatten()
                .any(|index| *index >= mesh.vertices.len())
        {
            return Err(format!("invalid terrain mesh {}", mesh.name));
        }
        self.primitives
            .extend(
                mesh.triangles
                    .iter()
                    .enumerate()
                    .map(|(index, triangle)| Primitive {
                        vertices: triangle.map(|vertex| mesh.vertices[vertex]),
                        key: TriangleKey {
                            source: source_id,
                            triangle: index as u32,
                        },
                    }),
            );
        self.sources.push(Source {
            location: source.clone(),
            mesh: mesh.name.clone(),
        });
        Ok(())
    }

    pub fn finish(mut self) -> TerrainIndex {
        let mut nodes = Vec::new();
        if !self.primitives.is_empty() {
            build_nodes(&mut self.primitives, 0, &mut nodes);
        }
        TerrainIndex {
            primitives: self.primitives,
            nodes,
            sources: self.sources,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct QueryStats {
    pub nodes_tested: usize,
    pub triangles_tested: usize,
}

#[derive(Debug)]
pub struct Query<T> {
    pub value: T,
    pub stats: QueryStats,
}

#[derive(Clone, Copy, Debug)]
pub struct SurfaceHit {
    pub height: f64,
    pub key: TriangleKey,
}

#[derive(Clone, Copy, Debug)]
pub struct RayHit {
    pub distance: f64,
    pub position: [f64; 3],
    pub normal: [f64; 3],
    pub key: TriangleKey,
}

#[derive(Clone, Copy, Debug)]
pub struct Ray {
    origin: [f64; 3],
    direction: [f64; 3],
    max_distance: f64,
}

impl Ray {
    pub fn new(origin: [f64; 3], direction: [f64; 3], max_distance: f64) -> Result<Self, String> {
        if origin
            .iter()
            .chain(&direction)
            .any(|value| !value.is_finite())
            || !max_distance.is_finite()
            || max_distance < 0.0
        {
            return Err(
                "ray coordinates and distance must be finite; distance must be nonnegative".into(),
            );
        }
        let direction = normalize(direction).ok_or("ray direction must be nonzero")?;
        Ok(Self {
            origin,
            direction,
            max_distance,
        })
    }
}

pub struct TerrainIndex {
    primitives: Vec<Primitive>,
    nodes: Vec<Node>,
    sources: Vec<Source>,
}

impl TerrainIndex {
    pub fn load_base(root: &Path) -> Result<Self, String> {
        let mut builder = Builder::default();
        let mut error = None;
        let survey = sab_terrain::retail::visit(root, |source, mesh| {
            if error.is_none() {
                error = builder.add(source, mesh).err();
            }
        })?;
        if let Some(error) = error {
            return Err(error);
        }
        if !survey.failures.is_empty() {
            return Err(format!(
                "incomplete terrain load: {} failures; first: {}",
                survey.failures.len(),
                survey.failures[0]
            ));
        }
        Ok(builder.finish())
    }

    pub fn triangle_count(&self) -> usize {
        self.primitives.len()
    }
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }
    pub fn source(&self, key: TriangleKey) -> Option<&Source> {
        self.sources.get(key.source as usize)
    }

    pub fn buffer_bytes(&self) -> usize {
        self.primitives.capacity() * std::mem::size_of::<Primitive>()
            + self.nodes.capacity() * std::mem::size_of::<Node>()
            + self.sources.capacity() * std::mem::size_of::<Source>()
    }

    pub fn heights_at(&self, world_x: f64, world_z: f64) -> Result<Query<Vec<SurfaceHit>>, String> {
        if !world_x.is_finite() || !world_z.is_finite() {
            return Err("height coordinates must be finite".into());
        }
        let mut query = Query {
            value: Vec::new(),
            stats: QueryStats::default(),
        };
        let mut stack = Vec::new();
        if !self.nodes.is_empty() {
            stack.push(0);
        }
        while let Some(index) = stack.pop() {
            let node = &self.nodes[index];
            query.stats.nodes_tested += 1;
            if ![0, 2]
                .into_iter()
                .zip([world_x, world_z])
                .all(|(axis, value)| {
                    value >= f64::from(node.bounds[0][axis]) - padding(node.bounds, axis)
                        && value <= f64::from(node.bounds[1][axis]) + padding(node.bounds, axis)
                })
            {
                continue;
            }
            match node.kind {
                NodeKind::Branch { left, right } => {
                    stack.push(right);
                    stack.push(left);
                }
                NodeKind::Leaf { start, end } => {
                    for primitive in &self.primitives[start..end] {
                        query.stats.triangles_tested += 1;
                        if let Some(height) = sab_terrain::geometry::triangle_height(
                            primitive.vertices,
                            world_x,
                            world_z,
                        ) {
                            query.value.push(SurfaceHit {
                                height,
                                key: primitive.key,
                            });
                        }
                    }
                }
            }
        }
        query.value.sort_by(|left, right| {
            left.height
                .total_cmp(&right.height)
                .then(left.key.cmp(&right.key))
        });
        Ok(query)
    }

    pub fn raycast(&self, ray: Ray) -> Query<Option<RayHit>> {
        let mut query = Query {
            value: None,
            stats: QueryStats::default(),
        };
        let mut stack = Vec::new();
        let mut limit = ray.max_distance;
        if !self.nodes.is_empty() {
            stack.push(0);
        }
        while let Some(index) = stack.pop() {
            let node = &self.nodes[index];
            query.stats.nodes_tested += 1;
            if ray_bounds(ray, node.bounds, limit).is_none() {
                continue;
            }
            match node.kind {
                NodeKind::Branch { left, right } => {
                    query.stats.nodes_tested += 2;
                    let left_distance = ray_bounds(ray, self.nodes[left].bounds, limit);
                    let right_distance = ray_bounds(ray, self.nodes[right].bounds, limit);
                    match (left_distance, right_distance) {
                        (Some(first), Some(second)) if first <= second => {
                            stack.push(right);
                            stack.push(left);
                        }
                        (Some(_), Some(_)) => {
                            stack.push(left);
                            stack.push(right);
                        }
                        (Some(_), None) => stack.push(left),
                        (None, Some(_)) => stack.push(right),
                        (None, None) => {}
                    }
                }
                NodeKind::Leaf { start, end } => {
                    for primitive in &self.primitives[start..end] {
                        query.stats.triangles_tested += 1;
                        if let Some(hit) = ray_triangle(ray, *primitive) {
                            let replace = query.value.as_ref().is_none_or(|current: &RayHit| {
                                hit.distance
                                    .total_cmp(&current.distance)
                                    .then(hit.key.cmp(&current.key))
                                    == Ordering::Less
                            });
                            if replace {
                                limit = hit.distance;
                                query.value = Some(hit);
                            }
                        }
                    }
                }
            }
        }
        query
    }
}

fn bounds(primitives: &[Primitive]) -> Bounds {
    let mut result = [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]];
    for vertex in primitives.iter().flat_map(|primitive| primitive.vertices) {
        for axis in 0..3 {
            result[0][axis] = result[0][axis].min(vertex[axis]);
            result[1][axis] = result[1][axis].max(vertex[axis]);
        }
    }
    result
}

fn build_nodes(primitives: &mut [Primitive], start: usize, nodes: &mut Vec<Node>) -> usize {
    let bounds = bounds(primitives);
    let index = nodes.len();
    nodes.push(Node {
        bounds,
        kind: NodeKind::Leaf {
            start,
            end: start + primitives.len(),
        },
    });
    if primitives.len() > LEAF_SIZE {
        let axis = (0..3)
            .max_by(|left, right| {
                (f64::from(bounds[1][*left]) - f64::from(bounds[0][*left]))
                    .total_cmp(&(f64::from(bounds[1][*right]) - f64::from(bounds[0][*right])))
            })
            .unwrap();
        let centroid = |primitive: &Primitive| {
            primitive
                .vertices
                .iter()
                .map(|vertex| f64::from(vertex[axis]))
                .sum::<f64>()
        };
        let middle = primitives.len() / 2;
        primitives.select_nth_unstable_by(middle, |left, right| {
            centroid(left)
                .total_cmp(&centroid(right))
                .then(left.key.cmp(&right.key))
        });
        let (left, right) = primitives.split_at_mut(middle);
        let left = build_nodes(left, start, nodes);
        let right = build_nodes(right, start + middle, nodes);
        nodes[index].kind = NodeKind::Branch { left, right };
    }
    index
}

fn padding(bounds: Bounds, axis: usize) -> f64 {
    (f64::from(bounds[1][axis]) - f64::from(bounds[0][axis])) * 2e-8 + 1e-9
}

fn ray_bounds(ray: Ray, bounds: Bounds, limit: f64) -> Option<f64> {
    let mut near = 0.0f64;
    let mut far = limit;
    for axis in 0..3 {
        let lower = f64::from(bounds[0][axis]) - padding(bounds, axis);
        let upper = f64::from(bounds[1][axis]) + padding(bounds, axis);
        if ray.direction[axis] == 0.0 {
            if ray.origin[axis] < lower || ray.origin[axis] > upper {
                return None;
            }
        } else {
            let first = (lower - ray.origin[axis]) / ray.direction[axis];
            let second = (upper - ray.origin[axis]) / ray.direction[axis];
            near = near.max(first.min(second));
            far = far.min(first.max(second));
            if near > far {
                return None;
            }
        }
    }
    Some(near)
}

fn subtract(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|axis| left[axis] - right[axis])
}
fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left.into_iter()
        .zip(right)
        .map(|(first, second)| first * second)
        .sum()
}
fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}
fn normalize(vector: [f64; 3]) -> Option<[f64; 3]> {
    let scale = vector.into_iter().map(f64::abs).fold(0.0f64, f64::max);
    if scale == 0.0 {
        return None;
    }
    let scaled = vector.map(|value| value / scale);
    let length = dot(scaled, scaled).sqrt();
    Some(scaled.map(|value| value / length))
}

fn ray_triangle(ray: Ray, primitive: Primitive) -> Option<RayHit> {
    let [first, second, third] = primitive.vertices.map(|vertex| vertex.map(f64::from));
    let edge_first = subtract(second, first);
    let edge_second = subtract(third, first);
    let normal = normalize(cross(edge_first, edge_second))?;
    if dot(normal, ray.direction).abs() < 1e-12 {
        return None;
    }
    let perpendicular = cross(ray.direction, edge_second);
    let determinant = dot(edge_first, perpendicular);
    let offset = subtract(ray.origin, first);
    let weight_second = dot(offset, perpendicular) / determinant;
    let crossed = cross(offset, edge_first);
    let weight_third = dot(ray.direction, crossed) / determinant;
    if !weight_second.is_finite()
        || !weight_third.is_finite()
        || weight_second < -1e-8
        || weight_third < -1e-8
        || weight_second + weight_third > 1.0 + 1e-8
    {
        return None;
    }
    let distance = dot(normal, subtract(first, ray.origin)) / dot(normal, ray.direction);
    if !distance.is_finite() || distance < 0.0 || distance > ray.max_distance {
        return None;
    }
    let distance = if distance == 0.0 { 0.0 } else { distance };
    let position =
        std::array::from_fn(|axis| ray.direction[axis].mul_add(distance, ray.origin[axis]));
    if position.iter().any(|value| !value.is_finite()) {
        return None;
    }
    Some(RayHit {
        distance,
        position,
        normal,
        key: primitive.key,
    })
}
