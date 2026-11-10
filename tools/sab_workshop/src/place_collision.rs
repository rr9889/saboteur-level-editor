use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use sab_tools::collision::CellCollision;
use transform_gizmo_egui::math::{DMat4, DVec3, DVec4};

use crate::placements_view::View3d;

const MAX_TRIANGLES: usize = 20_000;

pub struct Overlay {
    enabled: bool,
    cell_text: String,
    game: PathBuf,
    state: LoadState,
    frame: FrameStats,
}

enum LoadState {
    Idle,
    Pending {
        cell: u32,
        receiver: Receiver<Result<LoadedCell, String>>,
    },
    Ready(LoadedCell),
    Failed(String),
}

struct Layer {
    hash: u32,
    visible: bool,
    surfaces: Vec<usize>,
    triangles: usize,
}

struct LoadedCell {
    data: Snapshot,
    layers: Vec<Layer>,
    vertices: usize,
    triangles: usize,
    visible_surfaces: Vec<usize>,
    visible_triangles: usize,
}

struct Snapshot {
    cell: u32,
    source_pack: String,
    source_crc: u32,
    subpack_sha256: String,
    surfaces: Vec<WireSurface>,
    issues: Vec<String>,
    unsupported_records: usize,
}

struct WireSurface {
    layer: u32,
    vertices: Vec<[f32; 3]>,
    triangles: Vec<[u16; 3]>,
}

#[derive(Default, Debug, PartialEq)]
struct FrameStats {
    examined: usize,
    drawn: usize,
    edges: usize,
    culled: usize,
    invalid: usize,
    truncated: bool,
}

impl LoadedCell {
    fn new(data: CellCollision) -> Self {
        Self::from_snapshot(Snapshot {
            cell: data.cell,
            source_pack: data.source_pack,
            source_crc: data.source_crc,
            subpack_sha256: data.subpack_sha256,
            surfaces: data
                .surfaces
                .into_iter()
                .map(|surface| WireSurface {
                    layer: surface.layer,
                    vertices: surface.vertices,
                    triangles: surface.triangles,
                })
                .collect(),
            issues: data.issues,
            unsupported_records: data.unsupported_records,
        })
    }

    fn from_snapshot(data: Snapshot) -> Self {
        let mut layers = BTreeMap::new();
        let mut vertices = 0usize;
        let mut triangles = 0usize;
        for (index, surface) in data.surfaces.iter().enumerate() {
            vertices = vertices.saturating_add(surface.vertices.len());
            triangles = triangles.saturating_add(surface.triangles.len());
            let layer = layers.entry(surface.layer).or_insert_with(|| Layer {
                hash: surface.layer,
                visible: true,
                surfaces: Vec::new(),
                triangles: 0,
            });
            layer.surfaces.push(index);
            layer.triangles = layer.triangles.saturating_add(surface.triangles.len());
        }
        let mut loaded = Self {
            data,
            layers: layers.into_values().collect(),
            vertices,
            triangles,
            visible_surfaces: Vec::new(),
            visible_triangles: 0,
        };
        loaded.filter_layers();
        loaded
    }

    fn filter_layers(&mut self) {
        self.visible_surfaces.clear();
        self.visible_triangles = 0;
        for layer in &self.layers {
            if layer.visible {
                self.visible_triangles = self.visible_triangles.saturating_add(layer.triangles);
                self.visible_surfaces.extend(
                    layer
                        .surfaces
                        .iter()
                        .copied()
                        .filter(|&index| !self.data.surfaces[index].triangles.is_empty()),
                );
            }
        }
    }

    fn wireframe(
        &self,
        vp: &DMat4,
        rect: egui::Rect,
        budget: usize,
        mut emit: impl FnMut([egui::Pos2; 2]),
    ) -> FrameStats {
        let mut stats = FrameStats::default();
        'surfaces: for &index in &self.visible_surfaces {
            let surface = &self.data.surfaces[index];
            for triangle in &surface.triangles {
                if stats.examined >= budget {
                    break 'surfaces;
                }
                stats.examined += 1;
                let points = triangle.map(|vertex| surface.vertices.get(vertex as usize).copied());
                let [Some(first), Some(second), Some(third)] = points else {
                    stats.invalid += 1;
                    continue;
                };
                let points = [first, second, third]
                    .map(|point| DVec3::new(point[0] as f64, point[1] as f64, point[2] as f64));
                if points.iter().any(|point| !point.is_finite()) {
                    stats.invalid += 1;
                    continue;
                }
                let before = stats.edges;
                for [start, end] in [[0, 1], [1, 2], [2, 0]] {
                    if let Some(segment) = project_segment(vp, rect, points[start], points[end]) {
                        emit(segment);
                        stats.edges += 1;
                    }
                }
                if stats.edges > before {
                    stats.drawn += 1;
                } else {
                    stats.culled += 1;
                }
            }
        }
        stats.truncated = stats.examined < self.visible_triangles;
        stats
    }
}

fn clip_distances(point: DVec4) -> [f64; 7] {
    [
        point.w - 1e-5,
        point.x + point.w,
        point.w - point.x,
        point.y + point.w,
        point.w - point.y,
        point.z,
        point.w - point.z,
    ]
}

fn project_segment(
    vp: &DMat4,
    rect: egui::Rect,
    start: DVec3,
    end: DVec3,
) -> Option<[egui::Pos2; 2]> {
    let first = *vp * start.extend(1.0);
    let second = *vp * end.extend(1.0);
    if !first.is_finite() || !second.is_finite() {
        return None;
    }
    let mut enter = 0.0f64;
    let mut leave = 1.0f64;
    for (before, after) in clip_distances(first)
        .into_iter()
        .zip(clip_distances(second))
    {
        if before < 0.0 && after < 0.0 {
            return None;
        }
        if before < 0.0 {
            enter = enter.max(before / (before - after));
        } else if after < 0.0 {
            leave = leave.min(before / (before - after));
        }
        if enter > leave {
            return None;
        }
    }
    let projected = [
        View3d::project(vp, rect, start.lerp(end, enter))?,
        View3d::project(vp, rect, start.lerp(end, leave))?,
    ];
    projected
        .iter()
        .all(|point| point.x.is_finite() && point.y.is_finite())
        .then_some(projected)
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            enabled: false,
            cell_text: "66974465".into(),
            game: PathBuf::new(),
            state: LoadState::Idle,
            frame: FrameStats::default(),
        }
    }

    pub fn sync_game(&mut self, game: &Path) {
        if self.game != game {
            self.game = game.to_path_buf();
            self.state = LoadState::Idle;
            self.enabled = false;
            self.frame = FrameStats::default();
        }
    }

    fn load(&mut self) {
        if self.game.as_os_str().is_empty() || matches!(self.state, LoadState::Pending { .. }) {
            return;
        }
        self.frame = FrameStats::default();
        let cell = match self.cell_text.trim().parse::<u32>() {
            Ok(cell) => cell,
            Err(_) => {
                self.state = LoadState::Failed("Cell ID must be a decimal u32.".into());
                return;
            }
        };
        let game = self.game.clone();
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name(format!("collision-{cell}"))
            .spawn(move || {
                let result = sab_tools::collision::inspect_cell(&game, cell).map(LoadedCell::new);
                let _ = sender.send(result);
            }) {
            Ok(_) => {
                self.state = LoadState::Pending { cell, receiver };
                self.enabled = true;
            }
            Err(error) => {
                self.state =
                    LoadState::Failed(format!("Could not start collision inspection: {error}"))
            }
        }
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        let LoadState::Pending { receiver, .. } = &self.state else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(loaded)) => self.state = LoadState::Ready(loaded),
            Ok(Err(error)) => self.state = LoadState::Failed(error),
            Err(TryRecvError::Disconnected) => {
                self.state = LoadState::Failed("Collision inspection worker disconnected.".into())
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return;
            }
        }
        ctx.request_repaint();
    }

    pub fn controls(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Collision inspection (read-only)").id_source("pl_collision").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.enabled, "collision X-RAY").on_hover_text("Display cached collision only; enabling does not load data.");
                ui.label("cell ID");
                ui.add(egui::TextEdit::singleline(&mut self.cell_text).desired_width(105.0));
                if ui.add_enabled(!self.game.as_os_str().is_empty() && !matches!(self.state, LoadState::Pending { .. }), egui::Button::new("load collision")).clicked() {
                    self.load();
                }
            });
            ui.label("Read-only X-RAY source snapshot, not live physics or editable geometry. Collision stays at its source location after visual moves; building-to-collider ownership is unknown.");
            ui.label("Only structurally decoded baked meshes with exact collision-class placements in this cell. Other variants remain explicit gaps, never solid geometry.");
            match &mut self.state {
                LoadState::Idle => {
                    ui.label("Not loaded. Choose a cell and press load collision; no automatic inspection.");
                }
                LoadState::Pending { cell, .. } => {
                    ui.label(format!("Inspecting cell {cell} on a worker thread..."));
                }
                LoadState::Failed(error) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("Collision unavailable: {error}"));
                }
                LoadState::Ready(loaded) => {
                    ui.label(format!("Cell {} | pack {} | CRC 0x{:08x}", loaded.data.cell, loaded.data.source_pack, loaded.data.source_crc));
                    ui.add(egui::Label::new(format!("Snapshot SHA-256: {}", loaded.data.subpack_sha256)).wrap());
                    ui.label(format!("{} surfaces | {} vertices | {} triangles | {} unsupported records | {} issues/gaps", loaded.data.surfaces.len(), loaded.vertices, loaded.triangles, loaded.data.unsupported_records, loaded.data.issues.len()));
                    ui.label("Static authoring layers != proven runtime activation. All shown by default; these controls affect collision surfaces only.");
                    let mut changed = false;
                    ui.horizontal(|ui| {
                        if ui.button("All collision layers").clicked() {
                            for layer in &mut loaded.layers {
                                layer.visible = true;
                            }
                            changed = true;
                        }
                        if ui.button("No collision layers").clicked() {
                            for layer in &mut loaded.layers {
                                layer.visible = false;
                            }
                            changed = true;
                        }
                    });
                    egui::ScrollArea::vertical().id_source("pl_collision_layers").max_height(130.0).show_rows(ui, 20.0, loaded.layers.len(), |ui, rows| {
                        for index in rows {
                            let layer = &mut loaded.layers[index];
                            changed |= ui.checkbox(&mut layer.visible, format!("0x{:08x} | {} surfaces | {} triangles", layer.hash, layer.surfaces.len(), layer.triangles)).changed();
                        }
                    });
                    if changed {
                        loaded.filter_layers();
                        self.frame = FrameStats::default();
                    }
                    ui.label(format!("Last frame: {} / {} visible-layer triangles examined, {} drawn, {} edges, {} culled, {} invalid. Budget {MAX_TRIANGLES} triangles / {} edges. {}", self.frame.examined, loaded.visible_triangles, self.frame.drawn, self.frame.edges, self.frame.culled, self.frame.invalid, MAX_TRIANGLES * 3, if self.frame.truncated { "TRUNCATED: budget reached; not all collision shown." } else { "" }));
                    if !loaded.data.issues.is_empty() {
                        egui::CollapsingHeader::new("Inspection issues / gaps").id_source("pl_collision_gaps").show(ui, |ui| {
                            egui::ScrollArea::vertical().id_source("pl_collision_issues").max_height(120.0).show_rows(ui, 20.0, loaded.data.issues.len(), |ui, rows| {
                                for index in rows {
                                    ui.add(egui::Label::new(&loaded.data.issues[index]).truncate()).on_hover_text(&loaded.data.issues[index]);
                                }
                            });
                        });
                    }
                }
            }
        });
    }

    pub fn draw(&mut self, painter: &egui::Painter, last: Option<(egui::Rect, DMat4)>) {
        self.frame = FrameStats::default();
        if !self.enabled {
            return;
        }
        let (Some((rect, vp)), LoadState::Ready(loaded)) = (last, &self.state) else {
            return;
        };
        let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 225, 235));
        self.frame = loaded.wireframe(&vp, rect, MAX_TRIANGLES, |segment| {
            painter.line_segment(segment, stroke);
        });
        let label = format!("COLLISION X-RAY | source cell {}{}\n{} tris / {} edges | {} unsupported / {} issues/gaps\nRead-only, not live physics; static layers != runtime activation", loaded.data.cell, if self.frame.truncated { " | TRUNCATED (20k budget)" } else { "" }, self.frame.drawn, self.frame.edges, loaded.data.unsupported_records, loaded.data.issues.len());
        let position = rect.left_top() + egui::vec2(6.0, 6.0);
        let galley = painter.layout(
            label,
            egui::FontId::monospace(10.0),
            stroke.color,
            (rect.width() - 12.0).max(1.0),
        );
        painter.rect_filled(
            egui::Rect::from_min_size(
                position - egui::vec2(3.0, 3.0),
                galley.size() + egui::vec2(6.0, 6.0),
            ),
            egui::Rounding::same(2.0),
            egui::Color32::from_black_alpha(190),
        );
        painter.galley(position, galley, stroke.color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(layer: u32, triangles: usize) -> WireSurface {
        WireSurface {
            layer,
            vertices: vec![[-0.5, -0.5, 0.5], [0.5, -0.5, 0.5], [0.0, 0.5, 0.5]],
            triangles: vec![[0, 1, 2]; triangles],
        }
    }

    fn cell(surfaces: Vec<WireSurface>) -> LoadedCell {
        LoadedCell::from_snapshot(Snapshot {
            cell: 66974465,
            source_pack: "synthetic".into(),
            source_crc: 123,
            subpack_sha256: "synthetic".into(),
            surfaces,
            issues: vec!["unsupported variant omitted".into()],
            unsupported_records: 1,
        })
    }

    fn rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(400.0, 300.0))
    }

    #[test]
    fn defaults_and_display_toggle_do_not_load_collision() {
        let mut overlay = Overlay::new();
        assert!(!overlay.enabled);
        assert_eq!(overlay.cell_text, "66974465");
        overlay.sync_game(Path::new("synthetic-game"));
        overlay.enabled = true;
        overlay.poll(&egui::Context::default());
        assert!(matches!(overlay.state, LoadState::Idle));
    }

    #[test]
    fn headless_painter_only_emits_cached_edges_when_enabled_with_a_projection() {
        let mut overlay = Overlay::new();
        overlay.state = LoadState::Ready(cell(vec![surface(0, 1)]));
        let ctx = egui::Context::default();
        let mut paint = |enabled, last| {
            overlay.enabled = enabled;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(rect()),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        overlay.controls(ui);
                        overlay.draw(&ui.painter_at(rect()), last);
                    });
                },
            );
            output
                .shapes
                .iter()
                .filter(|shape| matches!(shape.shape, egui::Shape::LineSegment { .. }))
                .count()
        };
        assert_eq!(paint(false, Some((rect(), DMat4::IDENTITY))), 0);
        assert_eq!(paint(true, None), 0);
        assert_eq!(paint(true, Some((rect(), DMat4::IDENTITY))), 3);
        assert!(matches!(overlay.state, LoadState::Ready(_)));
    }

    #[test]
    fn layers_default_to_all_and_filter_only_collision_without_moving_vertices() {
        let mut loaded = cell(vec![surface(0xabcd, 2), surface(0, 1), surface(0xabcd, 0)]);
        assert_eq!(
            loaded
                .layers
                .iter()
                .map(|layer| layer.hash)
                .collect::<Vec<_>>(),
            vec![0, 0xabcd]
        );
        assert!(loaded.layers.iter().all(|layer| layer.visible));
        assert_eq!(loaded.visible_triangles, 3);
        assert_eq!(loaded.visible_surfaces.len(), 2);
        let source = loaded.data.surfaces[0].vertices.clone();
        loaded.layers[0].visible = false;
        loaded.filter_layers();
        assert_eq!(loaded.visible_surfaces, vec![0]);
        assert_eq!(loaded.visible_triangles, 2);
        assert_eq!(loaded.data.surfaces[0].vertices, source);
        assert_eq!(
            loaded
                .wireframe(&DMat4::IDENTITY, rect(), MAX_TRIANGLES, |_| {})
                .edges,
            6
        );
        loaded.layers[1].visible = false;
        loaded.filter_layers();
        assert_eq!(
            loaded.wireframe(&DMat4::IDENTITY, rect(), MAX_TRIANGLES, |_| {}),
            FrameStats::default()
        );
    }

    #[test]
    fn face_and_edge_work_is_bounded_and_reports_exact_truncation() {
        let loaded = cell(vec![surface(0, MAX_TRIANGLES + 1)]);
        let mut edges = 0;
        let stats = loaded.wireframe(&DMat4::IDENTITY, rect(), MAX_TRIANGLES, |_| edges += 1);
        assert_eq!(stats.examined, MAX_TRIANGLES);
        assert_eq!(stats.drawn, MAX_TRIANGLES);
        assert_eq!(stats.edges, MAX_TRIANGLES * 3);
        assert_eq!(edges, stats.edges);
        assert!(stats.truncated);
        let exact = cell(vec![surface(0, 2)]).wireframe(&DMat4::IDENTITY, rect(), 2, |_| {});
        assert!(!exact.truncated);
    }

    #[test]
    fn offscreen_and_invalid_faces_consume_the_budget() {
        let mut outside = surface(0, MAX_TRIANGLES + 1);
        for vertex in &mut outside.vertices {
            vertex[0] += 10.0;
        }
        let stats = cell(vec![outside]).wireframe(&DMat4::IDENTITY, rect(), MAX_TRIANGLES, |_| {
            panic!("offscreen edge")
        });
        assert_eq!(stats.examined, MAX_TRIANGLES);
        assert_eq!(stats.culled, MAX_TRIANGLES);
        assert!(stats.truncated);
        let mut invalid = surface(0, 2);
        invalid.triangles[0] = [0, 1, 9];
        invalid.vertices[2][1] = f32::NAN;
        let stats =
            cell(vec![invalid]).wireframe(&DMat4::IDENTITY, rect(), 1, |_| panic!("invalid edge"));
        assert_eq!(stats.invalid, 1);
        assert_eq!(stats.examined, 1);
        assert!(stats.truncated);
        let mut nonfinite = surface(0, 1);
        nonfinite.vertices[0][0] = f32::INFINITY;
        assert_eq!(
            cell(vec![nonfinite])
                .wireframe(&DMat4::IDENTITY, rect(), 1, |_| {})
                .invalid,
            1
        );
    }

    #[test]
    fn clipping_rejects_outside_planes_and_retains_crossing_edges() {
        for outside in [
            DVec3::new(-2.0, 0.0, 0.5),
            DVec3::new(2.0, 0.0, 0.5),
            DVec3::new(0.0, -2.0, 0.5),
            DVec3::new(0.0, 2.0, 0.5),
            DVec3::new(0.0, 0.0, -0.5),
            DVec3::new(0.0, 0.0, 2.0),
        ] {
            assert!(project_segment(&DMat4::IDENTITY, rect(), outside, outside).is_none());
        }
        let segment = project_segment(
            &DMat4::IDENTITY,
            rect(),
            DVec3::new(-2.0, 0.0, 0.5),
            DVec3::new(2.0, 0.0, 0.5),
        )
        .unwrap();
        assert_eq!(
            segment,
            [
                egui::pos2(rect().left(), rect().center().y),
                egui::pos2(rect().right(), rect().center().y)
            ]
        );
        let perspective = DMat4::perspective_rh(70f64.to_radians(), 1.0, 0.1, 5000.0);
        assert!(project_segment(&perspective, rect(), DVec3::Z, DVec3::Z * 2.0).is_none());
        let crossing = project_segment(&perspective, rect(), DVec3::Z, -DVec3::Z).unwrap();
        assert!(crossing.iter().all(|point| rect().contains(*point)));
    }

    #[test]
    fn path_changes_drop_cached_layers_status_and_late_worker_results() {
        let mut overlay = Overlay::new();
        overlay.sync_game(Path::new("first-game"));
        overlay.enabled = true;
        overlay.state = LoadState::Ready(cell(vec![surface(0, 1)]));
        overlay.frame.examined = 1;
        overlay.sync_game(Path::new("first-game"));
        assert!(matches!(overlay.state, LoadState::Ready(_)));
        assert!(overlay.enabled);
        overlay.sync_game(Path::new("second-game"));
        assert!(matches!(overlay.state, LoadState::Idle));
        assert!(!overlay.enabled);
        assert_eq!(overlay.frame, FrameStats::default());
        let (sender, receiver) = mpsc::channel();
        overlay.state = LoadState::Pending {
            cell: 66974465,
            receiver,
        };
        overlay.sync_game(Path::new("third-game"));
        assert!(sender.send(Ok(cell(vec![surface(0, 1)]))).is_err());
        overlay.poll(&egui::Context::default());
        assert!(matches!(overlay.state, LoadState::Idle));
    }

    #[test]
    fn worker_errors_and_disconnects_are_explicit() {
        let mut overlay = Overlay::new();
        let (sender, receiver) = mpsc::channel();
        overlay.state = LoadState::Pending { cell: 42, receiver };
        sender.send(Err("synthetic inspection gap".into())).unwrap();
        overlay.poll(&egui::Context::default());
        assert!(
            matches!(&overlay.state, LoadState::Failed(error) if error == "synthetic inspection gap")
        );
        let (sender, receiver) = mpsc::channel();
        overlay.state = LoadState::Pending { cell: 42, receiver };
        drop(sender);
        overlay.poll(&egui::Context::default());
        assert!(
            matches!(&overlay.state, LoadState::Failed(error) if error.contains("disconnected"))
        );
    }

    #[test]
    fn successful_worker_results_replace_the_pending_cell_without_loading_another() {
        let mut overlay = Overlay::new();
        let (sender, receiver) = mpsc::channel();
        overlay.state = LoadState::Pending {
            cell: 66974465,
            receiver,
        };
        overlay.cell_text = "42".into();
        sender.send(Ok(cell(vec![surface(7, 1)]))).unwrap();
        overlay.poll(&egui::Context::default());
        let LoadState::Ready(loaded) = &overlay.state else {
            panic!("worker not collected")
        };
        assert_eq!(loaded.data.cell, 66974465);
        assert_eq!(loaded.layers[0].hash, 7);
        assert_eq!(overlay.cell_text, "42");
    }

    #[test]
    fn invalid_cell_input_clears_stale_geometry_without_starting_a_worker() {
        let mut overlay = Overlay::new();
        overlay.sync_game(Path::new("synthetic-game"));
        overlay.state = LoadState::Ready(cell(vec![surface(0, 1)]));
        overlay.cell_text = "4294967296".into();
        overlay.load();
        assert!(matches!(&overlay.state, LoadState::Failed(error) if error.contains("u32")));
        overlay.sync_game(Path::new(""));
        overlay.load();
        assert!(matches!(overlay.state, LoadState::Idle));
    }
}
