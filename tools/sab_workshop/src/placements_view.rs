//! The places page's 3D view: the selected EditNodes objects and their neighbours (every EditNodes
//! object within a radius, across containers), drawn with egui's painter through a perspective camera,
//! with a translate gizmo (`transform-gizmo-egui`) on the selection and terrain-cell props as read-only
//! context.
//!
//! Controls follow Ariane's viewport scheme (Dryxio's GTA map editor; studied in
//! `docs/level_editor/_audit/group_C_ariane.md`, reimplemented here): LMB-drag look, MMB-drag pan,
//! Alt+MMB orbit, Ctrl+Alt+MMB zoom, Ctrl+Alt+LMB dolly, wheel = field of view (Alt+MMB click resets),
//! WASD fly with Shift fast / Alt slow; click select with Shift add / Alt remove / Ctrl toggle;
//! Shift+LMB-drag rectangle select; Shift while dragging the gizmo snaps; L toggles local/world axes.
//!
//! Objects whose ClassName resolves to a mesh (`place_mesh`) are drawn as that mesh when a full build
//! is open; every other object is a 1 m marker (EditNodes carries a position and an axis basis but no
//! bounds). Y is up. The view never edits anything itself; it returns a
//! [`ViewAction`] and the page applies it through the session, so one gizmo drag is one undo step.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use sab_formats::editnodes::{tags, Node};
use sab_tools::session::Session;
use transform_gizmo_egui::math::{DMat3, DMat4, DQuat, DVec3, DVec4, Transform};
use transform_gizmo_egui::{enum_set, Gizmo, GizmoConfig, GizmoExt, GizmoMode, GizmoOrientation};

use crate::cell_props::Loading;
use crate::gui::theme;

pub type ObjRef = (u32, usize);

enum SurfacePick {
    Object(ObjRef),
    Cell(crate::cell_props::CellTarget),
}

/// Intersect the actual triangles in instance space. Keeping the transformed direction unnormalised
/// preserves world-ray distances even when the instance is scaled.
fn mesh_hit(geom: &crate::place_mesh::Geom, model: DMat4, origin: DVec3, direction: DVec3) -> Option<f64> {
    let inverse = model.inverse();
    if !inverse.is_finite() { return None; }
    let o = inverse.transform_point3(origin);
    let d = inverse.transform_vector3(direction);
    let mut nearest = f64::INFINITY;
    for triangle in &geom.tris {
        let [a, b, c] = triangle.map(v3);
        let (ab, ac) = (b - a, c - a);
        let p = d.cross(ac);
        let det = ab.dot(p);
        if det.abs() < 1e-12 { continue; }
        let offset = o - a;
        let u = offset.dot(p) / det;
        if !(0.0..=1.0).contains(&u) { continue; }
        let q = offset.cross(ab);
        let v = d.dot(q) / det;
        if v < 0.0 || u + v > 1.0 { continue; }
        let distance = ac.dot(q) / det;
        if distance > 1e-4 && distance < nearest { nearest = distance; }
    }
    nearest.is_finite().then_some(nearest)
}

fn pick_surface(best: &mut Option<(f64, SurfacePick)>, ray: Option<(DVec3, DVec3)>, geom: &crate::place_mesh::Geom,
    model: DMat4, target: SurfacePick, occluder: Option<&crate::place_occlude::Index>) {
    let Some((o, d)) = ray else { return };
    let Some(distance) = mesh_hit(geom, model, o, d) else { return };
    if best.as_ref().map_or(false, |(old, _)| *old <= distance) { return; }
    if occluder.map_or(false, |index| index.blocked(o.to_array(), (o + d * distance).to_array(), HIDDEN_TOLERANCE)) { return; }
    *best = Some((distance, target));
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PickMode {
    Replace,
    Add,
    Remove,
    Toggle,
}

/// What the view asks the page to do.
#[derive(Debug, PartialEq)]
pub enum ViewAction {
    OpenCell(crate::cell_props::CellTarget),
    /// A click: `None` with `Replace` clears the selection.
    Pick(Option<ObjRef>, PickMode),
    /// A rectangle selection.
    PickMany(Vec<ObjRef>, PickMode),
    /// New positions for one or more objects, applied as one undo step.
    Move(Vec<(u32, usize, [f32; 3])>),
    /// Rotate objects by a row-major 3×3 matrix about a pivot, as one undo step.
    Rotate(Vec<ObjRef>, [[f32; 3]; 3], [f32; 3]),
}

struct Marker {
    obj: ObjRef,
    pos: DVec3,
    type_name: String,
    label: String,
    class_name: String,
    /// The stored XAxis/YAxis/ZAxis (identity when absent), as the model matrix's columns.
    axes: [[f64; 3]; 3],
}

#[derive(Clone, Copy, PartialEq)]
enum LmbDrag {
    Look,
    Dolly,
    Rect(egui::Pos2),
    Gizmo,
}

/// G/R's mouse transform: move or rotate the selection after a key press, with world-axis
/// constraints, snapping, and an explicit commit. It never mutates the session; the page receives one
/// `ViewAction` on commit, so one modal operation is one undo step.
#[derive(Clone, Copy, PartialEq)]
enum ModalKind {
    Move,
    Rotate,
}

#[derive(Clone, Copy, PartialEq)]
enum ModalAxis {
    Free,
    X,
    Y,
    Z,
}

struct ModalTransform {
    kind: ModalKind,
    axis: ModalAxis,
    start_pointer: egui::Pos2,
    /// The selection's stored positions and bases when the operation started.
    originals: Vec<(ObjRef, DVec3, DQuat)>,
    pivot: DVec3,
    start_ray: (DVec3, DVec3),
}

/// The default field of view, and the one Alt+MMB click restores.
pub const FOV_DEFAULT: f64 = 70.0;
/// Fly speed in metres per second (Shift ×4, Alt ×0.25).
const FLY_SPEED: f64 = 12.0;
/// The most cell props drawn at once (nearest first).
const MAX_PROPS: usize = 4000;
/// Gizmo snap increments offered (metres).
pub const SNAP_STEPS: [f32; 5] = [0.25, 0.5, 1.0, 2.0, 5.0];

pub struct View3d {
    yaw: f64,
    pitch: f64,
    dist: f64,
    target: DVec3,
    pub fov: f64,
    /// Centre on the primary selection at the next frame (set on list picks and jumps).
    focus_pending: bool,
    pub radius: f32,
    pub snap: f32,
    pub local_axes: bool,
    /// Gizmo mode: rotate (Q) instead of translate (W).
    pub rotate_mode: bool,
    gizmo: Gizmo,
    /// Gizmo drag in progress: the positions shown while dragging, committed on release.
    preview: Vec<(ObjRef, DVec3)>,
    /// The rotation shown while a rotate drag is in progress.
    preview_rot: Option<DQuat>,
    modal: Option<ModalTransform>,
    lmb: Option<LmbDrag>,
    cache_key: Option<(u64, u64, u32)>,
    /// The camera target the markers were gathered around; moving a quarter radius away regathers.
    cache_centre: DVec3,
    markers: Vec<Marker>,
    /// The viewport and view-projection of the last drawn frame.
    last: Option<(egui::Rect, DMat4)>,
    /// Terrain-cell placements drawn as read-only context.
    pub props: Loading,
    pub show_props: bool,
    props_near: Vec<u32>,
    edit_cell: Option<u32>,
    toasts: Vec<(String, f64)>,
    /// Real meshes for objects whose ClassName resolves to one; `None` when no full build is open.
    pub meshes: Option<crate::place_mesh::MeshLib>,
    /// Diffuse textures for the editable meshes' materials (same France.materials chain as the room);
    /// built on a worker thread because resolving a few hundred materials reads the packs.
    mesh_tex: Option<std::sync::Arc<crate::place_tex::TextureSet>>,
    mesh_tex_rx: Option<std::sync::mpsc::Receiver<crate::place_tex::TextureSet>>,
    mesh_tex_mats: std::collections::BTreeSet<u32>,
    /// Why meshes are not shown, when they are not.
    pub mesh_note: String,
    pub show_meshes: bool,
    pub show_markers: bool,
    /// Triangles and objects drawn as meshes in the last frame.
    mesh_stats: (usize, usize),
    /// Terrain-cell props drawn as meshes: (props, triangles, classes with no mesh).
    pub prop_mesh_stats: (usize, usize, usize),
    prop_log: (usize, usize, usize),
    /// The read-only room around the view centre (France map objects).
    pub world: crate::place_world::World,
    pub show_room: bool,
    /// Draw the room's diffuse textures (off: flat tinted pieces, for comparison).
    pub show_textures: bool,
    /// The texture set given to the GPU, with the objects' sets it was made from.
    tex_cache: Option<(Vec<usize>, Option<std::sync::Arc<crate::place_tex::TextureSet>>)>,
    /// Pick through the room's walls (off: hidden objects can't be clicked or box-selected).
    pub through_walls: bool,
    /// Line-of-sight index over the room's triangles, rebuilt when the room changes.
    occlude: Option<crate::place_occlude::Index>,
    /// Whether each marker is hidden from the eye by the room, with what it was computed for.
    hidden: HashMap<ObjRef, bool>,
    hidden_key: Option<([u64; 4], (usize, usize))>,
    game_dir: String,
    /// The texture the app renders the GPU scene into; `None` falls back to egui-painted meshes
    /// without depth (and nothing of the room).
    pub gpu_tex: Option<egui::TextureId>,
    /// This frame's GPU scene, for the app to render.
    pub gpu_request: Option<crate::place_gpu::DrawList>,
    /// Terrain preview geometry (key serial + triangles) drawn in the GPU scene when set; read-only context, never part of the edit session.
    terrain_geom: Option<(u64, std::sync::Arc<crate::place_mesh::Geom>)>,
    collision: crate::place_collision::Overlay,
}

/// The most mesh triangles drawn per frame (nearest objects first).
const MAX_MESH_TRIS: usize = 150_000;
/// Terrain-cell prop meshes: how many are drawn, and their own triangle budget.
const MAX_PROP_MESHES: usize = 400; // meshes drawn; lights, locators and other no-mesh classes do not count
const MAX_PROP_TRIS: usize = 600_000;

/// How far before an object's origin a room surface may lie without hiding it: furniture origins sit
/// on the floor they stand on.
const HIDDEN_TOLERANCE: f64 = 0.35;

fn v3(p: [f32; 3]) -> DVec3 {
    DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64)
}

fn v3_64(p: [f64; 3]) -> DVec3 {
    DVec3::new(p[0], p[1], p[2])
}

fn f3(p: DVec3) -> [f32; 3] {
    [p.x as f32, p.y as f32, p.z as f32]
}

fn type_colour(t: &str) -> egui::Color32 {
    match t {
        "Spore" => theme::RED,
        "Point" | "Locator" => theme::COLD,
        "" => theme::FAINT,
        _ => theme::DIM,
    }
}

fn movable(o: &Node) -> bool {
    o.params_with_tag(tags::POSITION).len() == 1
}

/// The object's orientation from its XAxis/YAxis/ZAxis leaves (identity when absent).
fn basis(o: &Node) -> DQuat {
    match (o.vec3_param(tags::X_AXIS), o.vec3_param(tags::Y_AXIS), o.vec3_param(tags::Z_AXIS)) {
        (Some(x), Some(y), Some(z)) => {
            let m = DMat3::from_cols(v3(x).normalize_or_zero(), v3(y).normalize_or_zero(), v3(z).normalize_or_zero());
            let q = DQuat::from_mat3(&m);
            if q.is_finite() && (q.length() - 1.0).abs() < 1e-3 {
                q.normalize()
            } else {
                DQuat::IDENTITY
            }
        }
        _ => DQuat::IDENTITY,
    }
}

impl View3d {
    pub fn new() -> View3d {
        View3d {
            yaw: 0.8,
            pitch: 0.35,
            dist: 9.0,
            target: DVec3::ZERO,
            fov: FOV_DEFAULT,
            focus_pending: true,
            radius: 60.0,
            snap: 1.0,
            local_axes: false,
            rotate_mode: false,
            gizmo: Gizmo::default(),
            preview: Vec::new(),
            preview_rot: None,
            modal: None,
            lmb: None,
            cache_key: None,
            cache_centre: DVec3::ZERO,
            markers: Vec::new(),
            last: None,
            props: Loading::Idle,
            show_props: true,
            props_near: Vec::new(),
            edit_cell: None,
            toasts: Vec::new(),
            meshes: None,
            mesh_tex: None,
            mesh_tex_rx: None,
            mesh_tex_mats: std::collections::BTreeSet::new(),
            mesh_note: "no full build open".into(),
            show_meshes: true,
            show_markers: false,
            mesh_stats: (0, 0),
            prop_mesh_stats: (0, 0, 0),
            prop_log: (9, 9, 9),
            world: crate::place_world::World::Idle,
            show_room: true,
            show_textures: true,
            tex_cache: None,
            through_walls: false,
            occlude: None,
            hidden: HashMap::new(),
            hidden_key: None,
            game_dir: String::new(),
            gpu_tex: None,
            gpu_request: None,
            terrain_geom: None,
            collision: crate::place_collision::Overlay::new(),
        }
    }

    /// Show a short message in the view's corner for a few seconds.
    pub fn toast(&mut self, ctx: &egui::Context, msg: impl Into<String>) {
        let until = ctx.input(|i| i.time) + 2.5;
        self.toasts.push((msg.into(), until));
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    /// The camera target (where Ctrl+V pastes).
    pub fn target(&self) -> [f32; 3] {
        f3(self.target)
    }

    /// Centre the camera on the primary selection at the next frame.
    pub fn request_focus(&mut self) {
        self.focus_pending = true;
    }

    /// The rectangle the 3D view occupied in the last drawn frame.
    pub fn viewport(&self) -> Option<egui::Rect> {
        self.last.map(|(r, _)| r)
    }

    /// Move the camera to look at `p` from `dist` metres (an area opened from the browser). Replaces any
    /// pending focus, so the view stays on the area rather than snapping to the selection.
    pub fn goto(&mut self, p: [f32; 3], dist: f64) {
        self.target = v3(p);
        self.dist = dist;
        self.focus_pending = false;
        self.cache_key = None;
    }

    /// Start loading the terrain-cell props for `game` once, when they are shown.
    pub fn ensure_props(&mut self, game: &str) {
        self.collision.sync_game(std::path::Path::new(game));
        if self.game_dir != game {
            self.game_dir = game.to_string();
            self.world = crate::place_world::World::Idle;
            self.props = Loading::Idle;
            self.markers.clear();
            self.cache_key = None;
            self.mesh_tex = None;
            self.mesh_tex_rx = None;
            self.mesh_tex_mats.clear();
        }
        if self.show_props && matches!(self.props, Loading::Idle) && !game.is_empty() {
            self.props = Loading::start(std::path::PathBuf::from(game));
        }
        let was_pending = matches!(self.props, Loading::Pending(_));
        self.props.poll();
        if was_pending && matches!(self.props, Loading::Ready(_)) {
            self.cache_key = None;
        }
    }

    /// Unit vector from the target to the eye.
    fn back(&self) -> DVec3 {
        let (cp, sp) = (self.pitch.cos(), self.pitch.sin());
        DVec3::new(self.yaw.cos() * cp, sp, self.yaw.sin() * cp)
    }

    fn eye(&self) -> DVec3 {
        self.target + self.back() * self.dist
    }

    fn matrices(&self, rect: egui::Rect) -> (DMat4, DMat4) {
        let view = DMat4::look_at_rh(self.eye(), self.target, DVec3::Y);
        let aspect = (rect.width() / rect.height().max(1.0)) as f64;
        let proj = DMat4::perspective_rh(self.fov.to_radians(), aspect, 0.1, 5000.0);
        (view, proj)
    }

    /// Screen position of a world point, or `None` behind the camera.
    pub(crate) fn project(vp: &DMat4, rect: egui::Rect, p: DVec3) -> Option<egui::Pos2> {
        let c = *vp * DVec4::new(p.x, p.y, p.z, 1.0);
        if c.w <= 1e-6 {
            return None;
        }
        let (x, y) = (c.x / c.w, c.y / c.w);
        Some(egui::pos2(rect.left() + ((x + 1.0) * 0.5) as f32 * rect.width(), rect.top() + ((1.0 - y) * 0.5) as f32 * rect.height()))
    }

    /// Forget the camera centre and the marker cache (a different session was opened).
    pub fn invalidate(&mut self) {
        self.focus_pending = true;
        self.cache_key = None;
        self.preview.clear();
        self.lmb = None;
        self.modal = None;
    }

    pub fn set_edit_cell(&mut self, cell: Option<u32>) {
        if self.edit_cell != cell {
            self.edit_cell = cell;
            self.cache_key = None;
        }
    }

    fn refresh_markers(&mut self, s: &Session, selected: &[ObjRef]) {
        let mut h = DefaultHasher::new();
        selected.hash(&mut h);
        let key = (h.finish(), s.revision(), self.radius.to_bits());
        if self.cache_key == Some(key) && self.cache_centre.distance(self.target) < self.radius as f64 * 0.25 {
            return;
        }
        self.cache_key = Some(key);
        self.cache_centre = self.target;
        self.markers.clear();
        let r2 = (self.radius as f64).powi(2);
        let centre = self.target;
        for c in s.containers() {
            let Some(e) = s.entry(c.hash) else { continue };
            for (i, o) in e.objects().iter().enumerate() {
                let Some(p) = o.position() else { continue };
                let p = v3(p);
                if p.distance_squared(centre) > r2 && !selected.contains(&(c.hash, i)) {
                    continue;
                }
                let name = s.object_name(o.tag()).map(|n| n.rsplit('\\').next().unwrap_or(&n).to_string());
                let axes = match (o.vec3_param(tags::X_AXIS), o.vec3_param(tags::Y_AXIS), o.vec3_param(tags::Z_AXIS)) {
                    (Some(x), Some(y), Some(z)) => [x, y, z].map(|a| a.map(|v| v as f64)),
                    _ => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                };
                self.markers.push(Marker {
                    obj: (c.hash, i),
                    pos: p,
                    type_name: o.type_name().unwrap_or_default(),
                    label: format!("{} · {}", name.unwrap_or_else(|| format!("0x{:08x}", o.tag())), o.class_name().unwrap_or_default()),
                    class_name: o.class_name().unwrap_or_default(),
                    axes,
                });
            }
        }
        self.props_near = match &self.props {
            Loading::Ready(cp) if self.show_props => cp.near(f3(centre), self.radius, MAX_PROPS).into_iter().filter(|index| Some(cp.props[*index as usize].cell) != self.edit_cell).collect(),
            _ => Vec::new(),
        };
    }

    fn controls_row(&mut self, ui: &mut egui::Ui, primary_pos: DVec3) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Focus").on_hover_text("Centre the camera on the primary selection").clicked() {
                self.target = primary_pos;
                self.cache_key = None;
            }
            ui.label(theme::data_text("snap", 10.0, theme::FAINT)).on_hover_text("Hold Shift while dragging the gizmo to snap");
            egui::ComboBox::from_id_source("pl_snap").selected_text(format!("{} m", self.snap)).width(64.0).show_ui(ui, |ui| {
                for s in SNAP_STEPS {
                    ui.selectable_value(&mut self.snap, s, format!("{s} m"));
                }
            });
            ui.selectable_value(&mut self.rotate_mode, false, "Move").on_hover_text("Translate gizmo (W)");
            ui.selectable_value(&mut self.rotate_mode, true, "Rotate").on_hover_text("Rotate gizmo (Q); Shift snaps to 15°. A 90° yaw was demonstrated in game on a locator; tilts were not");
            ui.selectable_value(&mut self.local_axes, false, "World").on_hover_text("Gizmo axes (L toggles)");
            ui.selectable_value(&mut self.local_axes, true, "Local").on_hover_text("Gizmo axes (L toggles)");
        });
        egui::CollapsingHeader::new(theme::data_text("view options and status", 10.0, theme::FAINT)).id_source("pl_view_opts").default_open(false).show(ui, |ui| {
            self.view_options(ui);
        });
    }

    /// Display toggles and the status lines the view reports about what it drew.
    fn view_options(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::data_text("radius", 10.0, theme::FAINT));
            ui.add(egui::Slider::new(&mut self.radius, 10.0..=500.0).suffix(" m").logarithmic(true)).on_hover_text("Show EditNodes objects and cell props within this distance");
            if ui.checkbox(&mut self.show_props, "cell props").on_hover_text("Effective mounted terrain-cell placements. Double-click an origin marker to request editing its cell.").changed() {
                self.cache_key = None;
            }
            ui.checkbox(&mut self.show_meshes, "meshes").on_hover_text(
                "Real meshes for objects whose ClassName names a GameTemplates asset (Dynamic0 meshes); everything else stays a marker",
            );
            ui.checkbox(&mut self.show_markers, "object markers").on_hover_text("Show locator boxes and cell origin dots. Selected objects remain highlighted; hover reveals nearby origins for picking.");
            ui.checkbox(&mut self.show_room, "room").on_hover_text(
                "Read-only geometry of the France map object (interior) containing the view centre, as its own placement list places it; GPU view only",
            );
            ui.checkbox(&mut self.show_textures, "textures").on_hover_text(
                "Diffuse textures on the room (France.materials → DTEX). Off: flat tinted pieces. Spec/normal maps and the game's lighting are not applied",
            );
            ui.checkbox(&mut self.through_walls, "through walls").on_hover_text(
                "Off: objects hidden behind the room's walls, floors or roof are dimmed and can't be clicked or box-selected. On: select anything",
            );
        });
        let meshes = match (&self.meshes, self.show_meshes) {
            (_, false) => "meshes off".to_string(),
            (Some(_), true) => format!(
                "meshes {} objects, {} tris; map props {} drawn ({} tris), {} classes without a mesh",
                self.mesh_stats.1, self.mesh_stats.0, self.prop_mesh_stats.0, self.prop_mesh_stats.1, self.prop_mesh_stats.2
            ),
            (None, true) => format!("meshes not available: {}", self.mesh_note),
        };
        ui.label(theme::data_text(meshes, 10.0, theme::FAINT));
        if self.show_props && self.show_meshes {
            ui.add(egui::Label::new(theme::data_text("Cell buildings: effective pack, intact coarse LOD; all static layers. Double-click a mesh surface or origin marker to edit (experimental opt-in). Visuals are not collision.", 9.5, theme::FAINT)).wrap());
        }
        if self.show_meshes {
            let tex = match (&self.mesh_tex, &self.mesh_tex_rx) {
                (_, Some(_)) => "object/cell textures: resolving…".to_string(),
                (Some(t), None) => format!("object/cell textures: {}", t.report),
                (None, None) => "object/cell textures: none resolved yet".to_string(),
            };
            ui.add(egui::Label::new(theme::data_text(tex, 9.5, theme::FAINT)).wrap());
        }
        let room = match (self.show_room, self.gpu_tex.is_some()) {
            (false, _) => "room: off".to_string(),
            (true, false) => "room: needs the GPU view (not available here)".to_string(),
            (true, true) => self.world.status(),
        };
        ui.add(egui::Label::new(theme::data_text(room, 10.0, theme::FAINT)).wrap());
        self.collision.controls(ui);
        let state = match &self.props {
            Loading::Idle => String::new(),
            Loading::Pending(_) => "loading…".into(),
            Loading::Ready(cp) => format!("{}/{}", self.props_near.len(), cp.props.len()),
            Loading::Failed(e) => format!("not available: {e}"),
        };
        ui.label(theme::data_text(format!("{} objects · cell props {state} · FOV {:.0}°", self.markers.len(), self.fov), 10.0, theme::FAINT));
    }

    /// The texture set for the GPU this frame: the objects' sets (merged when several have textures),
    /// or `None` when textures are off or nothing resolved.
    fn gpu_textures(&mut self) -> Option<std::sync::Arc<crate::place_tex::TextureSet>> {        if !self.show_textures {
            return None;
        }
        let objs = self.world.objects();
        let key: Vec<usize> = objs.iter().map(|o| std::sync::Arc::as_ptr(&o.tex) as usize).collect();
        if self.tex_cache.as_ref().map(|(k, _)| k) != Some(&key) {
            let useful: Vec<&std::sync::Arc<crate::place_tex::TextureSet>> = objs.iter().map(|o| &o.tex).filter(|t| t.is_useful()).collect();
            let set = match useful.len() {
                0 => None,
                1 => Some(useful[0].clone()),
                _ => Some(std::sync::Arc::new(crate::place_tex::TextureSet::merge(&useful.iter().map(|t| &***t).collect::<Vec<_>>()))),
            };
            self.tex_cache = Some((key, set));
        }
        self.tex_cache.as_ref().and_then(|(_, s)| s.clone())
    }

    /// Collect a finished editable-mesh texture resolve (worker thread).
    fn poll_mesh_textures(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.mesh_tex_rx else { return };
        match rx.try_recv() {
            Ok(ts) => {
                self.mesh_tex = Some(std::sync::Arc::new(ts));
                self.mesh_tex_rx = None;
                ctx.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(150)),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.mesh_tex_rx = None,
        }
    }

    /// Start a background resolve for the editable meshes' materials once their geometry has stopped
    /// growing. The room's materials go through the same chain; the GPU pass takes the union.
    fn start_mesh_textures(&mut self, ctx: &egui::Context) {
        if self.mesh_tex_rx.is_some() || self.game_dir.is_empty() {
            return;
        }
        let Some(lib) = self.meshes.as_ref() else { return };
        if lib.pending() {
            return;
        }
        let mats = lib.materials();
        if mats.is_empty() || mats == self.mesh_tex_mats {
            return;
        }
        self.mesh_tex_mats = mats.clone();
        let cells = lib.texture_cells();
        let game = std::path::PathBuf::from(&self.game_dir);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let ts = crate::place_tex::resolve_cells(&game, &cells, &mats);
            let _ = tx.send(ts);
        });
        self.mesh_tex_rx = Some(rx);
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    /// The draw list's texture set: the room's and the editable meshes' resolved materials, or either.
    fn combined_textures(&mut self, room: Option<std::sync::Arc<crate::place_tex::TextureSet>>) -> Option<std::sync::Arc<crate::place_tex::TextureSet>> {
        match (room, self.mesh_tex.clone()) {
            (Some(r), Some(m)) => Some(std::sync::Arc::new(crate::place_tex::TextureSet::merge(&[&r, &m]))),
            (r, None) => r,
            (None, Some(m)) => Some(m),
        }
    }

    /// Whether a geometry's material ranges get at least one resolved diffuse texture.
    fn is_textured(geom: &crate::place_mesh::Geom, tex: Option<&std::sync::Arc<crate::place_tex::TextureSet>>) -> bool {
        tex.map_or(false, |ts| geom.ranges.iter().any(|r| ts.mat_tex.get(&r.material).map_or(false, |h| ts.tex.contains_key(h))))
    }

    /// Recompute which markers the room hides from the eye. Cached on the eye, the markers' positions
    /// and the room's contents, so a still camera costs nothing.
    fn update_hidden(&mut self) {
        let objs: &[crate::place_world::Object] = if self.show_room { self.world.objects() } else { &[] };
        let sig = (
            objs.iter().map(|o| o.pieces.len()).sum::<usize>(),
            objs.iter().flat_map(|o| &o.pieces).map(|p| p.geom.tris.len()).sum::<usize>(),
        );
        if sig.0 == 0 {
            self.occlude = None;
            self.hidden.clear();
            self.hidden_key = None;
            return;
        }
        if self.occlude.as_ref().map(|i| i.signature()) != Some(sig) {
            self.occlude = Some(crate::place_occlude::Index::build(objs));
            self.hidden_key = None;
        }
        let eye = self.eye();
        let mut h = DefaultHasher::new();
        for m in &self.markers {
            for v in m.pos.to_array() {
                v.to_bits().hash(&mut h);
            }
        }
        let key = ([eye.x.to_bits(), eye.y.to_bits(), eye.z.to_bits(), h.finish()], sig);
        if self.hidden_key == Some(key) {
            return;
        }
        self.hidden_key = Some(key);
        self.hidden.clear();
        if let Some(index) = &self.occlude {
            for m in &self.markers {
                self.hidden.insert(m.obj, index.blocked(eye.to_array(), m.pos.to_array(), HIDDEN_TOLERANCE));
            }
        }
    }

    /// Whether the room hides this object from the eye (always false when picking through walls).
    fn is_hidden(&self, obj: ObjRef) -> bool {
        !self.through_walls && self.hidden.get(&obj).copied().unwrap_or(false)
    }

    /// Whether a G/R mouse transform is in progress. The page asks so Esc cancels the transform
    /// instead of clearing the selection.
    pub fn modal_active(&self) -> bool {
        self.modal.is_some()
    }

    /// A screen point's world ray: origin on the near plane and unit direction.
    /// The world ray (origin, unit direction) under a screen point, from the last drawn frame's camera; `None` outside the view or
    /// before the first frame. Read-only: used for terrain picking under the cursor.
    /// The screen rectangle the view was last drawn into (None before the first frame).
    #[cfg(test)]
    pub fn last_rect(&self) -> Option<egui::Rect> {
        self.last.map(|(r, _)| r)
    }

    /// The terrain preview geometry for the GPU scene (None hides it). The serial must change whenever the geometry does.
    pub fn set_terrain(&mut self, g: Option<(u64, std::sync::Arc<crate::place_mesh::Geom>)>) {
        self.terrain_geom = g;
    }

    /// The terrain preview in the view: its serial and triangle count (tests).
    #[cfg(test)]
    pub fn terrain_state(&self) -> Option<(u64, usize)> {
        self.terrain_geom.as_ref().map(|(s, g)| (*s, g.tris.len()))
    }

    /// The camera's look-at point (world).
    pub fn target_xyz(&self) -> [f32; 3] {
        f3(self.target)
    }

    pub fn hover_ray(&self, p: egui::Pos2) -> Option<(DVec3, DVec3)> {
        let (rect, vp) = self.last?;
        if !rect.contains(p) {
            return None;
        }
        let (o, d) = Self::screen_ray(&vp, rect, p);
        (d.length_squared() > 0.0).then_some((o, d))
    }

    fn screen_ray(vp: &DMat4, rect: egui::Rect, p: egui::Pos2) -> (DVec3, DVec3) {
        let x = ((p.x - rect.left()) / rect.width().max(1.0)) * 2.0 - 1.0;
        let y = 1.0 - ((p.y - rect.top()) / rect.height().max(1.0)) * 2.0;
        let inv = vp.inverse();
        let near = inv * DVec4::new(x as f64, y as f64, -1.0, 1.0);
        let far = inv * DVec4::new(x as f64, y as f64, 1.0, 1.0);
        let near = near.truncate() / near.w;
        let far = far.truncate() / far.w;
        (near, (far - near).normalize_or_zero())
    }

    /// Where a ray meets the plane through `pivot` with normal `n`.
    fn ray_plane(pivot: DVec3, n: DVec3, o: DVec3, d: DVec3) -> Option<DVec3> {
        let den = d.dot(n);
        if den.abs() < 1e-6 {
            return None;
        }
        Some(o + d * ((pivot - o).dot(n) / den))
    }

    /// The axis-line parameter closest to a ray. Used for X/Y/Z-constrained grabs.
    fn ray_axis_t(pivot: DVec3, axis: DVec3, o: DVec3, d: DVec3) -> f64 {
        let w = o - pivot;
        let b = axis.dot(d);
        let den = 1.0 - b * b;
        if den.abs() < 1e-6 {
            0.0
        } else {
            (axis.dot(w) - b * d.dot(w)) / den
        }
    }

    fn axis_vec(a: ModalAxis) -> DVec3 {
        match a {
            ModalAxis::X => DVec3::X,
            ModalAxis::Y => DVec3::Y,
            ModalAxis::Z => DVec3::Z,
            // The rotate default: yaw about the world up axis, the rotation kind demonstrated in game.
            ModalAxis::Free => DVec3::Y,
        }
    }

    fn start_modal(&mut self, ctx: &egui::Context, kind: ModalKind, selected: &[ObjRef], s: &Session, pointer: egui::Pos2, rect: egui::Rect, vp: &DMat4) {
        let originals: Vec<(ObjRef, DVec3, DQuat)> = selected
            .iter()
            .filter_map(|r| {
                let ob = s.entry(r.0)?.objects().get(r.1)?;
                Some((*r, v3(ob.position()?), basis(ob)))
            })
            .collect();
        if originals.is_empty() {
            self.toast(ctx, "G/R needs a selection with a Position");
            return;
        }
        let n = originals.len() as f64;
        let pivot = originals.iter().fold(DVec3::ZERO, |a, (_, p, _)| a + *p) / n;
        self.preview = originals.iter().map(|(r, p, _)| (*r, *p)).collect();
        self.preview_rot = None;
        self.modal = Some(ModalTransform { kind, axis: ModalAxis::Free, start_pointer: pointer, originals, pivot, start_ray: Self::screen_ray(vp, rect, pointer) });
    }

    /// Update the mouse transform for this frame and commit/cancel it. `action` is set on commit.
    fn update_modal(&mut self, ui: &egui::Ui, painter: &egui::Painter, rect: egui::Rect, vp: &DMat4, action: &mut Option<ViewAction>) {
        let Some(mut modal) = self.modal.take() else { return };
        let (m, pointer) = ui.input(|i| (i.modifiers, i.pointer.latest_pos().unwrap_or(modal.start_pointer)));
        // Axis constraints: X/Y/Z, pressed again to release.
        for (key, ax) in [(egui::Key::X, ModalAxis::X), (egui::Key::Y, ModalAxis::Y), (egui::Key::Z, ModalAxis::Z)] {
            if ui.input(|i| i.key_pressed(key)) {
                modal.axis = if modal.axis == ax { ModalAxis::Free } else { ax };
            }
        }
        let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape) || i.pointer.secondary_clicked());
        let confirm = ui.input(|i| i.key_pressed(egui::Key::Enter) || i.pointer.primary_clicked());
        let (o, d) = Self::screen_ray(vp, rect, pointer);
        let mut delta = DVec3::ZERO;
        let mut q = DQuat::IDENTITY;
        match modal.kind {
            ModalKind::Move => match modal.axis {
                ModalAxis::Free => {
                    let n = -self.back();
                    if let (Some(p0), Some(p1)) = (Self::ray_plane(modal.pivot, n, modal.start_ray.0, modal.start_ray.1), Self::ray_plane(modal.pivot, n, o, d)) {
                        delta = p1 - p0;
                    }
                }
                ax => {
                    let a = Self::axis_vec(ax);
                    let t0 = Self::ray_axis_t(modal.pivot, a, modal.start_ray.0, modal.start_ray.1);
                    let t1 = Self::ray_axis_t(modal.pivot, a, o, d);
                    delta = a * (t1 - t0);
                }
            },
            ModalKind::Rotate => {
                let a = Self::axis_vec(modal.axis);
                let mut angle = match (
                    Self::ray_plane(modal.pivot, a, modal.start_ray.0, modal.start_ray.1),
                    Self::ray_plane(modal.pivot, a, o, d),
                ) {
                    (Some(p0), Some(p1)) => {
                        let (v0, v1) = (p0 - modal.pivot, p1 - modal.pivot);
                        if v0.length_squared() > 1e-8 && v1.length_squared() > 1e-8 {
                            v0.cross(v1).dot(a).atan2(v0.dot(v1))
                        } else {
                            0.0
                        }
                    }
                    // Looking almost along the axis: use the pointer's screen angle as the fallback.
                    _ => {
                        let Some(ps) = Self::project(vp, rect, modal.pivot) else { return };
                        let pp = pointer;
                        let s = modal.start_pointer;
                        ((pp.y - ps.y).atan2(pp.x - ps.x) - (s.y - ps.y).atan2(s.x - ps.x)) as f64
                    }
                };
                if m.shift {
                    let step = 15f64.to_radians();
                    angle = (angle / step).round() * step;
                }
                q = DQuat::from_axis_angle(a, angle);
            }
        }
        if m.shift && modal.kind == ModalKind::Move {
            let step = self.snap as f64;
            if step > 0.0 {
                delta = match modal.axis {
                    ModalAxis::Free => DVec3::new((delta.x / step).round() * step, (delta.y / step).round() * step, (delta.z / step).round() * step),
                    ax => Self::axis_vec(ax) * ((delta.dot(Self::axis_vec(ax)) / step).round() * step),
                };
            }
        }
        self.preview = modal
            .originals
            .iter()
            .map(|(r, p, _)| (*r, if modal.kind == ModalKind::Move { *p + delta } else { modal.pivot + q * (*p - modal.pivot) }))
            .collect();
        self.preview_rot = (modal.kind == ModalKind::Rotate).then_some(q);

        // What is being done, near the pointer: kind, axis, amount.
        let axis_name = match modal.axis {
            ModalAxis::Free if modal.kind == ModalKind::Move => "view plane".to_string(),
            ModalAxis::Free => "Y".to_string(),
            ModalAxis::X => "X".to_string(),
            ModalAxis::Y => "Y".to_string(),
            ModalAxis::Z => "Z".to_string(),
        };
        let what = if modal.kind == ModalKind::Move {
            format!("G · {axis_name} · ({:.2}, {:.2}, {:.2}) m", delta.x, delta.y, delta.z)
        } else {
            format!("R · {axis_name} · {:.1}°", q.angle_between(DQuat::IDENTITY).to_degrees())
        };
        painter.text(pointer + egui::vec2(14.0, -14.0), egui::Align2::LEFT_BOTTOM, what, egui::FontId::monospace(11.0), theme::TX);
        if let Some(pp) = Self::project(vp, rect, modal.pivot) {
            painter.circle_stroke(pp, 5.0, egui::Stroke::new(1.5, theme::EMBER));
        }

        if cancel {
            self.preview.clear();
            self.preview_rot = None;
            self.modal = None;
            return;
        }
        if confirm {
            if modal.kind == ModalKind::Move {
                let moves: Vec<(u32, usize, [f32; 3])> = self
                    .preview
                    .iter()
                    .filter_map(|(r, p)| {
                        let orig = modal.originals.iter().find(|(q, _, _)| q == r)?.1;
                        (orig.distance(*p) > 1e-4).then_some((r.0, r.1, f3(*p)))
                    })
                    .collect();
                if !moves.is_empty() {
                    *action = Some(ViewAction::Move(moves));
                }
            } else if q.angle_between(DQuat::IDENTITY) > 1e-5 {
                let mt = DMat3::from_quat(q);
                let r = [0, 1, 2].map(|i| [0, 1, 2].map(|j| mt.col(j)[i] as f32));
                *action = Some(ViewAction::Rotate(modal.originals.iter().map(|(r, _, _)| *r).collect(), r, f3(modal.pivot)));
            }
            self.preview.clear();
            self.preview_rot = None;
            self.modal = None;
            return;
        }
        self.modal = Some(modal);
        ui.ctx().request_repaint();
    }

    /// Camera input over the viewport (Ariane's scheme).
    fn camera_input(&mut self, ui: &egui::Ui, resp: &egui::Response) {
        let (m, dt) = ui.input(|i| (i.modifiers, i.stable_dt.min(0.1) as f64));
        let ctrl_alt = m.ctrl && m.alt;
        if resp.dragged_by(egui::PointerButton::Primary) {
            let d = resp.drag_delta();
            match self.lmb {
                Some(LmbDrag::Look) => {
                    let eye = self.eye();
                    self.yaw += d.x as f64 * 0.005;
                    self.pitch = (self.pitch - d.y as f64 * 0.005).clamp(-1.5, 1.5);
                    self.target = eye - self.back() * self.dist;
                }
                Some(LmbDrag::Dolly) => self.target -= self.back() * (d.y as f64 * self.dist * 0.01),
                _ => {}
            }
        }
        if resp.dragged_by(egui::PointerButton::Middle) {
            let d = resp.drag_delta();
            if ctrl_alt {
                self.dist = (self.dist * (1.0 + d.y as f64 * 0.01)).clamp(1.5, 2000.0);
            } else if m.alt {
                self.yaw += d.x as f64 * 0.01;
                self.pitch = (self.pitch + d.y as f64 * 0.01).clamp(-1.5, 1.5);
            } else {
                let fwd = -self.back();
                let right = fwd.cross(DVec3::Y).normalize();
                let up = right.cross(fwd);
                self.target += (-right * d.x as f64 + up * d.y as f64) * (self.dist * 0.0025);
            }
        }
        // Right-drag orbit: kept from this view's first version (not part of Ariane's scheme).
        if resp.dragged_by(egui::PointerButton::Secondary) {
            let d = resp.drag_delta();
            self.yaw += d.x as f64 * 0.01;
            self.pitch = (self.pitch + d.y as f64 * 0.01).clamp(-1.5, 1.5);
        }
        if resp.clicked_by(egui::PointerButton::Middle) && m.alt && !m.ctrl {
            self.fov = FOV_DEFAULT;
        }
        if resp.hovered() {
            let scroll = ui.input(|i| i.raw_scroll_delta.y) as f64;
            if scroll != 0.0 {
                self.fov = (self.fov - scroll * 0.075).clamp(10.0, 120.0);
            }
        }
        if !ui.ctx().wants_keyboard_input() && !m.ctrl {
            let (fw, bk, lf, rt) = ui.input(|i| (i.key_down(egui::Key::W), i.key_down(egui::Key::S), i.key_down(egui::Key::A), i.key_down(egui::Key::D)));
            if fw || bk || lf || rt {
                let mul = if m.shift { 4.0 } else if m.alt { 0.25 } else { 1.0 };
                let fwd = -self.back();
                let right = fwd.cross(DVec3::Y).normalize();
                let mut v = DVec3::ZERO;
                if fw {
                    v += fwd;
                }
                if bk {
                    v -= fwd;
                }
                if rt {
                    v += right;
                }
                if lf {
                    v -= right;
                }
                self.target += v.normalize_or_zero() * FLY_SPEED * mul * dt;
                ui.ctx().request_repaint();
            }
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, s: &Session, primary: Option<ObjRef>, selected: &[ObjRef], height: f32) -> Option<ViewAction> {
        self.collision.poll(ui.ctx());
        let o = primary.and_then(|r| s.entry(r.0)?.objects().get(r.1));
        let position = o.and_then(|o| o.position()).map(v3);
        let primary_pos = position.unwrap_or(self.target);
        if self.focus_pending && position.is_some() {
            self.focus_pending = false;
            self.target = primary_pos;
            self.cache_key = None;
        }
        self.refresh_markers(s, selected);
        self.controls_row(ui, primary_pos);

        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, egui::Rounding::ZERO, theme::G0);
        let m = ui.input(|i| i.modifiers);
        let keys_free = !ui.ctx().wants_keyboard_input();
        let (view, proj) = self.matrices(rect);
        let vp = proj * view;
        self.last = Some((rect, vp));
        let mut action: Option<ViewAction> = None;

        // G/R (Ariane's mouse transforms): start from a key press over the view. The transform feeds
        // the same preview the gizmo uses and returns one ViewAction on commit, so it is one undo step.
        if keys_free && self.modal.is_none() && !m.ctrl && resp.hovered() && !selected.is_empty() {
            let pointer = ui.input(|i| i.pointer.latest_pos()).unwrap_or(rect.center());
            if ui.input(|i| i.key_pressed(egui::Key::G)) {
                self.start_modal(ui.ctx(), ModalKind::Move, selected, s, pointer, rect, &vp);
            } else if ui.input(|i| i.key_pressed(egui::Key::R)) {
                self.start_modal(ui.ctx(), ModalKind::Rotate, selected, s, pointer, rect, &vp);
            }
        }
        let modal_was_active = self.modal.is_some();
        if modal_was_active {
            self.update_modal(ui, &painter, rect, &vp, &mut action);
        }

        if self.modal.is_none() {
            // What a left drag does is decided when it starts: the gizmo if a handle is under the
            // pointer, a rectangle with Shift, a dolly with Ctrl+Alt, otherwise look.
            if resp.drag_started_by(egui::PointerButton::Primary) {
                let start = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()).unwrap_or(rect.center());
                self.lmb = Some(if self.gizmo.is_focused() {
                    LmbDrag::Gizmo
                } else if m.shift {
                    LmbDrag::Rect(start)
                } else if m.ctrl && m.alt {
                    LmbDrag::Dolly
                } else {
                    LmbDrag::Look
                });
            }
            self.camera_input(ui, &resp);
            if keys_free && ui.input(|i| i.key_pressed(egui::Key::L)) {
                self.local_axes = !self.local_axes;
                let msg = if self.local_axes { "Gizmo axes: local" } else { "Gizmo axes: world" };
                self.toast(ui.ctx(), msg);
            }
            // W translate / Q rotate (Ariane). W also flies forward while held; switching to translate
            // is silent, as in Ariane.
            if keys_free && self.lmb.is_none() && !m.ctrl {
                if ui.input(|i| i.key_pressed(egui::Key::W)) {
                    self.rotate_mode = false;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Q)) {
                    self.rotate_mode = true;
                    self.toast(ui.ctx(), "Gizmo: rotate");
                }
            }
        }

        // The GPU scene (room and editable meshes, depth-tested) is shown as an image under every
        // overlay; the app renders it after this UI frame, from `gpu_request`.
        let mut gpu_items: Option<Vec<crate::place_gpu::Item>> = None;
        let mut gpu_textures: Option<std::sync::Arc<crate::place_tex::TextureSet>> = None;
        if let Some(tex) = self.gpu_tex {
            painter.image(tex, rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            let mut items = Vec::new();
            if self.show_room {
                self.world.ensure(&self.game_dir, f3(self.target));
                if self.world.poll() {
                    ui.ctx().request_repaint();
                }
                if matches!(self.world, crate::place_world::World::Pending(..)) {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
                }
                gpu_textures = self.gpu_textures();
                for o in self.world.objects() {
                    for pc in &o.pieces {
                        // A piece with a resolved texture shows it unmodified (white tint); any other
                        // piece gets a small per-mesh tint so adjacent pieces read apart.
                        let textured = gpu_textures.as_ref().map_or(false, |ts| pc.geom.ranges.iter().any(|r| ts.mat_tex.get(&r.material).map_or(false, |h| ts.tex.contains_key(h))));
                        let colour = if textured {
                            [1.0, 1.0, 1.0]
                        } else {
                            let mut h = DefaultHasher::new();
                            pc.key.hash(&mut h);
                            let t = (h.finish() % 1000) as f32 / 1000.0;
                            [0.52 + 0.12 * t, 0.50 + 0.08 * (1.0 - t), 0.46 + 0.06 * t]
                        };
                        items.push(crate::place_gpu::Item { key: pc.key.clone(), geom: pc.geom.clone(), model: pc.model, colour });
                    }
                }
            }
            if let Some((serial, g)) = &self.terrain_geom {
                let id = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
                items.push(crate::place_gpu::Item { key: format!("terrain|{serial}"), geom: g.clone(), model: id, colour: [0.38, 0.54, 0.30] });
            }
            gpu_items = Some(items);
        }
        let room_drawn = gpu_items.as_ref().map_or(false, |i| !i.is_empty());
        self.update_hidden();

        // Ground grid on the primary selection's height, 5 m spacing (not over a drawn room).
        let span = if room_drawn { 0.0 } else { (self.radius as f64).min(120.0) };
        let step = 5.0;
        let n = (span / step) as i32;
        let base = DVec3::new((self.target.x / step).round() * step, primary_pos.y, (self.target.z / step).round() * step);
        let gstroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(0x22, 0x24, 0x28));
        for k in -n..=n {
            let off = k as f64 * step;
            for (a, b) in [
                (base + DVec3::new(off, 0.0, -span), base + DVec3::new(off, 0.0, span)),
                (base + DVec3::new(-span, 0.0, off), base + DVec3::new(span, 0.0, off)),
            ] {
                if let (Some(pa), Some(pb)) = (Self::project(&vp, rect, a), Self::project(&vp, rect, b)) {
                    painter.line_segment([pa, pb], gstroke);
                }
            }
        }

        let hover = resp.hover_pos();

        // Terrain-cell props: a dot on a 0.4 m stem, olive for the base layer, violet for a static
        // layer (occupation / resistance variants). Hover names them; double-click requests cell editing.
        let mut prop_hover: Option<(f32, u32)> = None;
        if let Loading::Ready(cp) = &self.props {
            let eye = self.eye();
            for &i in &self.props_near {
                let pr = &cp.props[i as usize];
                let p = v3(pr.pos);
                if p.distance(eye) < 3.0 {
                    continue;
                }
                let (Some(sp), Some(top)) = (Self::project(&vp, rect, p), Self::project(&vp, rect, p + DVec3::Y * 0.4)) else { continue };
                if !rect.contains(sp) {
                    continue;
                }
                let colour = if pr.layer == 0 { egui::Color32::from_rgb(0x6f, 0x76, 0x52) } else { egui::Color32::from_rgb(0x6c, 0x5c, 0x8a) };
                if self.show_markers || hover.map_or(false, |hp| hp.distance(top) < 8.0) {
                    painter.line_segment([sp, top], egui::Stroke::new(1.0, colour));
                    painter.circle_filled(top, 1.8, colour);
                }
                if let Some(hp) = hover {
                    let d = hp.distance(top);
                    if d < 8.0 && prop_hover.map_or(true, |(nd, _)| d < nd) {
                        prop_hover = Some((d, i));
                    }
                }
            }
        }
        if matches!(self.props, Loading::Pending(_)) {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }

        // Real meshes, nearest objects first up to the triangle budget, flat-shaded and painted back
        // to front. Objects drawn this way keep only their centre dot unless selected.
        let mut meshed: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut mesh_prov: HashMap<usize, String> = HashMap::new();
        self.mesh_stats = (0, 0);
        let eye = self.eye();
        let pick_ray = resp.clicked().then(|| hover.map(|p| Self::screen_ray(&vp, rect, p))).flatten();
        let mut surface_pick = None;
        let pick_occluder = if self.through_walls { None } else { self.occlude.as_ref() };
        if let (Some(lib), true) = (self.meshes.as_mut(), self.show_meshes) {
            if lib.poll() {
                ui.ctx().request_repaint();
            }
            let mut order: Vec<usize> = (0..self.markers.len()).collect();
            order.sort_by(|a, b| self.markers[*a].pos.distance_squared(eye).total_cmp(&self.markers[*b].pos.distance_squared(eye)));
            let mut tris: Vec<(f32, [egui::Pos2; 3], egui::Color32)> = Vec::new();
            let mut pending = false;
            let turn = self.preview_rot.unwrap_or(DQuat::IDENTITY);
            for k in order {
                let mk = &self.markers[k];
                if mk.class_name.is_empty() {
                    continue;
                }
                let slot = match self.edit_cell {
                    Some(cell) => lib.get_cell(std::path::Path::new(&self.game_dir), cell, &mk.class_name),
                    None => lib.get(&mk.class_name),
                };
                let g = match slot {
                    crate::place_mesh::Slot::Ready(g) => g.clone(),
                    crate::place_mesh::Slot::Pending => {
                        pending = true;
                        continue;
                    }
                    crate::place_mesh::Slot::None(why) => {
                        mesh_prov.insert(k, format!("no mesh: {why}"));
                        continue;
                    }
                };
                if self.mesh_stats.0 + g.tris.len() > MAX_MESH_TRIS {
                    continue;
                }
                let is_sel = selected.contains(&mk.obj);
                let textured = Self::is_textured(&g, self.mesh_tex.as_ref());
                let p = self.preview.iter().find(|(r, _)| *r == mk.obj).map(|(_, p)| *p).unwrap_or(mk.pos);
                let axes = if is_sel && self.preview_rot.is_some() { mk.axes.map(|a| (turn * DVec3::from(a)).to_array()) } else { mk.axes };
                let model = DMat4::from_cols(DVec3::from(axes[0]).extend(0.0), DVec3::from(axes[1]).extend(0.0), DVec3::from(axes[2]).extend(0.0), p.extend(1.0));
                pick_surface(&mut surface_pick, pick_ray, &g, model, SurfacePick::Object(mk.obj), pick_occluder);
                let base = if is_sel {
                    [0xe0, 0x8a, 0x4a]
                } else if textured {
                    [255, 255, 255]
                } else {
                    [0xb4, 0xae, 0x9e]
                };
                if let Some(items) = gpu_items.as_mut() {
                    let c = |a: [f64; 3]| [a[0] as f32, a[1] as f32, a[2] as f32, 0.0];
                    items.push(crate::place_gpu::Item {
                        key: match self.edit_cell {
                            Some(cell) => crate::place_cell_mesh::Key::new(cell, &mk.class_name).gpu_key(std::path::Path::new(&self.game_dir)),
                            None => format!("class|{}", mk.class_name),
                        },
                        geom: g.clone(),
                        model: [c(axes[0]), c(axes[1]), c(axes[2]), [p.x as f32, p.y as f32, p.z as f32, 1.0]],
                        colour: if is_sel {
                            [0.92, 0.50, 0.22]
                        } else if textured {
                            [1.0, 1.0, 1.0]
                        } else {
                            [0.30, 0.62, 0.78]
                        },
                    });
                    self.mesh_stats.0 += g.tris.len();
                    self.mesh_stats.1 += 1;
                    meshed.insert(k);
                    mesh_prov.insert(k, g.provenance.clone());
                    continue;
                }
                let light = DVec3::new(0.4, 0.85, 0.55).normalize();
                for (c, n) in g.tris.iter().zip(&g.normals) {
                    let w = c.map(|q| v3_64(crate::place_mesh::place(q, &axes, p.to_array())));
                    let mut sp = [egui::Pos2::ZERO; 3];
                    let mut depth = 0.0;
                    let mut ok = true;
                    for (j, q) in w.iter().enumerate() {
                        let cl = vp * DVec4::new(q.x, q.y, q.z, 1.0);
                        if cl.w <= 0.1 {
                            ok = false;
                            break;
                        }
                        sp[j] = egui::pos2(rect.left() + ((cl.x / cl.w + 1.0) * 0.5) as f32 * rect.width(), rect.top() + ((1.0 - cl.y / cl.w) * 0.5) as f32 * rect.height());
                        depth += cl.w;
                    }
                    if !ok || sp.iter().all(|s| !rect.expand(50.0).contains(*s)) {
                        continue;
                    }
                    let wn = crate::place_mesh::place(*n, &axes, [0.0; 3]);
                    let shade = 0.3 + 0.7 * v3_64(wn).normalize_or_zero().dot(light).abs();
                    let col = egui::Color32::from_rgb((base[0] as f64 * shade) as u8, (base[1] as f64 * shade) as u8, (base[2] as f64 * shade) as u8);
                    tris.push((depth as f32, sp, col));
                }
                self.mesh_stats.0 += g.tris.len();
                self.mesh_stats.1 += 1;
                meshed.insert(k);
                mesh_prov.insert(k, g.provenance.clone());
            }
            if pending {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            tris.sort_by(|a, b| b.0.total_cmp(&a.0));
            for chunk in tris.chunks(20_000) {
                let mut mesh = egui::Mesh::default();
                for (_, sp, col) in chunk {
                    let i = mesh.vertices.len() as u32;
                    for s in sp {
                        mesh.colored_vertex(*s, *col);
                    }
                    mesh.add_triangle(i, i + 1, i + 2);
                }
                painter.add(egui::Shape::mesh(mesh));
            }
        }

        // Terrain-cell props (outdoor buildings, vehicles, clutter) as real meshes in the GPU view:
        // nearest first, a separate triangle budget, repeated classes share one uploaded mesh. A class
        // with no resolvable mesh keeps its dot. Read-only: these are not selectable.
        self.prop_mesh_stats = (0, 0, 0);
        if let (Some(items), Some(lib), true, Loading::Ready(cp)) = (gpu_items.as_mut(), self.meshes.as_mut(), self.show_meshes, &self.props) {
            let mut tris = 0usize;
            let mut pending = false;
            for &i in self.props_near.iter() {
                if self.prop_mesh_stats.0 >= MAX_PROP_MESHES {
                    break;
                }
                let pr = &cp.props[i as usize];
                if pr.class_name.is_empty() {
                    continue;
                }
                let g = match lib.get_cell(std::path::Path::new(&self.game_dir), pr.cell, &pr.class_name) {
                    crate::place_mesh::Slot::Ready(g) => g.clone(),
                    crate::place_mesh::Slot::Pending => {
                        pending = true;
                        continue;
                    }
                    crate::place_mesh::Slot::None(why) => {
                        if std::env::var_os("SAB_WORKSHOP_TERRAIN_DEMO").is_some() && self.prop_mesh_stats.2 < 6 && self.prop_log.0 == 9 {
                            eprintln!("prop mesh none: {} -> {why}", pr.class_name);
                        }
                        self.prop_mesh_stats.2 += 1;
                        continue;
                    }
                };
                if tris + g.tris.len() > MAX_PROP_TRIS {
                    continue;
                }
                tris += g.tris.len();
                let a = pr.axes.unwrap_or([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
                let c = |v: [f32; 3]| [v[0], v[1], v[2], 0.0];
                let model = DMat4::from_cols(v3(a[0]).extend(0.0), v3(a[1]).extend(0.0), v3(a[2]).extend(0.0), v3(pr.pos).extend(1.0));
                pick_surface(&mut surface_pick, pick_ray, &g, model, SurfacePick::Cell(pr.target()), pick_occluder);
                items.push(crate::place_gpu::Item {
                    key: crate::place_cell_mesh::Key::new(pr.cell, &pr.class_name).gpu_key(std::path::Path::new(&self.game_dir)),
                    geom: g,
                    model: [c(a[0]), c(a[1]), c(a[2]), [pr.pos[0], pr.pos[1], pr.pos[2], 1.0]],
                    colour: [0.62, 0.56, 0.48],
                });
                self.prop_mesh_stats.0 += 1;
            }
            self.prop_mesh_stats.1 = tris;
            if std::env::var_os("SAB_WORKSHOP_TERRAIN_DEMO").is_some() && self.prop_mesh_stats != self.prop_log {
                self.prop_log = self.prop_mesh_stats;
                eprintln!("prop meshes: {} drawn, {} tris, {} none, near {}", self.prop_mesh_stats.0, tris, self.prop_mesh_stats.2, self.props_near.len());
            }
            if pending {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        }

        self.poll_mesh_textures(ui.ctx());
        self.start_mesh_textures(ui.ctx());
        let combined_textures = self.combined_textures(gpu_textures.take());
        if let Some(items) = gpu_items {
            let ppp = ui.ctx().pixels_per_point();
            self.gpu_request = Some(crate::place_gpu::DrawList {
                size: [(rect.width() * ppp).round() as u32, (rect.height() * ppp).round() as u32],
                view_proj: vp.to_cols_array_2d().map(|c| c.map(|v| v as f32)),
                items,
                textures: combined_textures,
            });
        }

        // Markers: a 1 m box per object, coloured by Type; selected objects in ember, the primary
        // one drawn heavier.
        self.collision.draw(&painter, self.last);
        let mut nearest: Option<(f32, usize)> = None;
        let mut screen: Vec<Option<egui::Pos2>> = Vec::with_capacity(self.markers.len());
        for (k, mk) in self.markers.iter().enumerate() {
            let is_sel = selected.contains(&mk.obj);
            let p = self.preview.iter().find(|(r, _)| *r == mk.obj).map(|(_, p)| *p).unwrap_or(mk.pos);
            let sp = Self::project(&vp, rect, p).filter(|sp| rect.contains(*sp));
            screen.push(sp);
            let Some(sp) = sp else { continue };
            // An object the room hides from the eye is dimmed and can't be picked (unless picking
            // through walls); a selected one stays highlighted.
            let hidden = !is_sel && self.is_hidden(mk.obj);
            let colour = if is_sel { theme::EMBER } else { type_colour(&mk.type_name) };
            let colour = if hidden { colour.gamma_multiply(0.3) } else { colour };
            let width = if Some(mk.obj) == primary { 2.5 } else if is_sel { 1.8 } else { 1.0 };
            if meshed.contains(&k) && !is_sel {
                if self.show_markers || hover.map_or(false, |hp| hp.distance(sp) < 12.0) {
                    painter.circle_filled(sp, 2.5, colour);
                }
                if let Some(hp) = hover {
                    let d = hp.distance(sp);
                    if !hidden && d < 12.0 && nearest.map_or(true, |(nd, _)| d < nd) {
                        nearest = Some((d, k));
                    }
                }
                continue;
            }
            if self.show_markers || is_sel || hover.map_or(false, |hp| hp.distance(sp) < 12.0) {
                let h = 0.5;
                let corners: Vec<Option<egui::Pos2>> = (0..8)
                    .map(|c| {
                        let d = DVec3::new(if c & 1 == 0 { -h } else { h }, if c & 2 == 0 { -h } else { h }, if c & 4 == 0 { -h } else { h });
                        Self::project(&vp, rect, p + d)
                    })
                    .collect();
                for (a, b) in [(0, 1), (2, 3), (4, 5), (6, 7), (0, 2), (1, 3), (4, 6), (5, 7), (0, 4), (1, 5), (2, 6), (3, 7)] {
                    if let (Some(pa), Some(pb)) = (corners[a], corners[b]) {
                        painter.line_segment([pa, pb], egui::Stroke::new(width, colour));
                    }
                }
                painter.circle_filled(sp, if is_sel { 3.5 } else { 2.0 }, colour);
            }
            if let Some(hp) = hover {
                let d = hp.distance(sp);
                if !hidden && d < 12.0 && nearest.map_or(true, |(nd, _)| d < nd) {
                    nearest = Some((d, k));
                }
            }
        }

        // The primary selection's own axis basis (XAxis red, YAxis green, ZAxis blue), 1.5 m long,
        // turned by the rotation being dragged.
        let basis_origin = self.preview.iter().find(|(r, _)| Some(*r) == primary).map(|(_, p)| *p).unwrap_or(primary_pos);
        let turn = self.preview_rot.unwrap_or(DQuat::IDENTITY);
        for (tag, colour) in [(tags::X_AXIS, egui::Color32::from_rgb(0xd0, 0x50, 0x50)), (tags::Y_AXIS, egui::Color32::from_rgb(0x50, 0xc0, 0x60)), (tags::Z_AXIS, egui::Color32::from_rgb(0x50, 0x80, 0xe0))] {
            if let Some(ax) = o.and_then(|o| o.vec3_param(tag)) {
                if let (Some(a), Some(b)) = (Self::project(&vp, rect, basis_origin), Self::project(&vp, rect, basis_origin + (turn * v3(ax)) * 1.5)) {
                    painter.line_segment([a, b], egui::Stroke::new(2.0, colour));
                }
            }
        }

        // Translate gizmo on every movable selected object. One drag = one undo step, on release.
        let targets: Vec<(ObjRef, DVec3, DQuat)> = selected
            .iter()
            .filter_map(|r| {
                let ob = s.entry(r.0)?.objects().get(r.1)?;
                if !movable(ob) {
                    return None;
                }
                let p = v3(ob.position()?);
                let shown = self.preview.iter().find(|(q, _)| q == r).map(|(_, p)| *p).unwrap_or(p);
                Some((*r, shown, basis(ob)))
            })
            .collect();
        if self.modal.is_none() && !targets.is_empty() {
            let modes = if self.rotate_mode {
                enum_set!(GizmoMode::RotateX | GizmoMode::RotateY | GizmoMode::RotateZ)
            } else {
                enum_set!(GizmoMode::TranslateX | GizmoMode::TranslateY | GizmoMode::TranslateZ | GizmoMode::TranslateXZ | GizmoMode::TranslateView)
            };
            self.gizmo.update_config(GizmoConfig {
                view_matrix: view.into(),
                projection_matrix: proj.into(),
                viewport: rect,
                modes,
                orientation: if self.local_axes { GizmoOrientation::Local } else { GizmoOrientation::Global },
                snapping: m.shift,
                snap_distance: self.snap,
                snap_angle: 15f32.to_radians(),
                ..Default::default()
            });
            // The gizmo applies each frame's delta to the transforms it is given, so a drag feeds back
            // its own accumulated preview (positions, and the rotation so far).
            let turn = self.preview_rot.unwrap_or(DQuat::IDENTITY);
            let ts: Vec<Transform> = targets.iter().map(|(_, p, q)| Transform::from_scale_rotation_translation(DVec3::ONE, turn * *q, *p)).collect();
            if let Some((_, moved)) = self.gizmo.interact(ui, &ts) {
                self.preview = targets.iter().zip(&moved).map(|((r, _, _), t)| (*r, DVec3::new(t.translation.x, t.translation.y, t.translation.z))).collect();
                if self.rotate_mode {
                    let q1: DQuat = moved[0].rotation.into();
                    self.preview_rot = Some((q1 * targets[0].2.inverse()).normalize());
                }
                self.lmb = Some(LmbDrag::Gizmo);
            }
        }
        let released = ui.input(|i| i.pointer.primary_released());

        // Rectangle selection overlay and commit.
        if let Some(LmbDrag::Rect(start)) = self.lmb {
            if let Some(cur) = ui.input(|i| i.pointer.latest_pos()) {
                let r = egui::Rect::from_two_pos(start, cur);
                painter.rect_filled(r, egui::Rounding::ZERO, egui::Color32::from_rgba_unmultiplied(100, 150, 255, 40));
                painter.rect_stroke(r, egui::Rounding::ZERO, egui::Stroke::new(1.5, egui::Color32::from_rgba_unmultiplied(100, 150, 255, 200)));
                if released {
                    let hits: Vec<ObjRef> = self
                        .markers
                        .iter()
                        .zip(&screen)
                        .filter(|(mk, sp)| sp.map_or(false, |p| r.contains(p)) && !self.is_hidden(mk.obj))
                        .map(|(mk, _)| mk.obj)
                        .collect();
                    let mode = if m.alt { PickMode::Remove } else if m.ctrl { PickMode::Add } else { PickMode::Replace };
                    action = Some(ViewAction::PickMany(hits, mode));
                }
            }
        }

        if released {
            if self.lmb == Some(LmbDrag::Gizmo) && self.rotate_mode {
                if let Some(q) = self.preview_rot.filter(|q| q.angle_between(DQuat::IDENTITY) > 1e-5) {
                    let mt = transform_gizmo_egui::math::DMat3::from_quat(q);
                    let r = [0, 1, 2].map(|i| [0, 1, 2].map(|j| mt.col(j)[i] as f32));
                    let n = targets.len().max(1) as f64;
                    let pivot = targets.iter().fold(DVec3::ZERO, |a, (rf, _, _)| a + s.entry(rf.0).and_then(|e| e.objects().get(rf.1)?.position()).map(v3).unwrap_or_default()) / n;
                    action = Some(ViewAction::Rotate(targets.iter().map(|(rf, _, _)| *rf).collect(), r, f3(pivot)));
                }
            } else if self.lmb == Some(LmbDrag::Gizmo) {
                let originals: Vec<(ObjRef, DVec3)> = targets.iter().map(|(r, _, _)| (*r, s.entry(r.0).and_then(|e| e.objects().get(r.1)?.position()).map(v3).unwrap_or_default())).collect();
                let moves: Vec<(u32, usize, [f32; 3])> = self
                    .preview
                    .iter()
                    .filter(|(r, p)| originals.iter().any(|(q, o)| q == r && o.distance(*p) > 1e-4))
                    .map(|(r, p)| (r.0, r.1, f3(*p)))
                    .collect();
                if !moves.is_empty() {
                    action = Some(ViewAction::Move(moves));
                }
            }
            self.preview.clear();
            self.preview_rot = None;
            self.lmb = None;
        }

        // Nudge (this editor's addition): arrows move along world X / Z, PageUp/PageDown along Y;
        // 0.1 m per press, 1 m with Shift. Each press is one undo step for the whole selection.
        if action.is_none() && self.modal.is_none() && self.lmb.is_none() && resp.hovered() && keys_free && !targets.is_empty() {
            let d = ui.input(|i| {
                let step = if i.modifiers.shift { 1.0 } else { 0.1 };
                let mut d = DVec3::ZERO;
                for (k, v) in [
                    (egui::Key::ArrowRight, DVec3::X),
                    (egui::Key::ArrowLeft, -DVec3::X),
                    (egui::Key::ArrowUp, -DVec3::Z),
                    (egui::Key::ArrowDown, DVec3::Z),
                    (egui::Key::PageUp, DVec3::Y),
                    (egui::Key::PageDown, -DVec3::Y),
                ] {
                    if i.key_pressed(k) {
                        d += v * step;
                    }
                }
                d
            });
            if d != DVec3::ZERO {
                action = Some(ViewAction::Move(targets.iter().map(|(r, p, _)| (r.0, r.1, f3(*p + d))).collect()));
            }
        }

        // Click (not on the gizmo): Shift adds, Alt removes, Ctrl toggles, plain replaces; a click on
        // empty space clears the selection.
        if action.is_none() && self.modal.is_none() && !modal_was_active && resp.clicked() && !self.gizmo.is_focused() {
            let hit = nearest.map(|(_, k)| self.markers[k].obj);
            let mode = if m.shift { PickMode::Add } else if m.alt { PickMode::Remove } else if m.ctrl { PickMode::Toggle } else { PickMode::Replace };
            let prop_wins = prop_hover.map_or(false, |(distance, _)| nearest.map_or(true, |(marker_distance, _)| distance < marker_distance));
            if let Some((_, surface)) = surface_pick {
                match surface {
                    SurfacePick::Object(object) => action = Some(ViewAction::Pick(Some(object), mode)),
                    SurfacePick::Cell(target) if resp.double_clicked() && mode == PickMode::Replace && !m.command => action = Some(ViewAction::OpenCell(target)),
                    SurfacePick::Cell(_) => {} // Context scenery is opened for editing on double-click.
                }
            } else if prop_wins && mode == PickMode::Replace && !m.command {
                if let (true, Some((_, index)), Loading::Ready(props)) = (resp.double_clicked(), prop_hover, &self.props) {
                    action = Some(ViewAction::OpenCell(props.props[index as usize].target()));
                }
            } else if hit.is_some() || mode == PickMode::Replace {
                action = Some(ViewAction::Pick(hit, mode));
            }
        }

        // Hover label: an EditNodes marker first, else a cell prop.
        if self.lmb.is_none() {
            if let Some((_, k)) = nearest {
                let mk = &self.markers[k];
                let label = match mesh_prov.get(&k) {
                    Some(p) => format!("{}\n{p}", mk.label),
                    None => mk.label.clone(),
                };
                painter.text(label_pos(&vp, rect, mk.pos), egui::Align2::LEFT_BOTTOM, label, egui::FontId::monospace(11.0), theme::TX);
            } else if let (Some((_, i)), Loading::Ready(cp)) = (prop_hover, &self.props) {
                let pr = &cp.props[i as usize];
                let layer = if pr.layer == 0 { String::new() } else { format!(" · layer 0x{:08x}", pr.layer) };
                let label = format!("cell prop · {} {} · cell {} · list {} · {:08x}{layer}\nDouble-click origin marker: edit this placement", pr.class_name, pr.type_name, pr.cell, pr.list, pr.tag);
                painter.text(label_pos(&vp, rect, v3(pr.pos) + DVec3::new(0.0, 0.2, 0.0)), egui::Align2::LEFT_BOTTOM, label, egui::FontId::monospace(11.0), theme::DIM);
            }
        }

        // Toasts, newest at the bottom of the top-right stack.
        let now = ui.input(|i| i.time);
        self.toasts.retain(|(_, until)| *until > now);
        for (k, (msg, _)) in self.toasts.iter().enumerate() {
            let pos = rect.right_top() + egui::vec2(-8.0, 8.0 + 18.0 * k as f32);
            let galley = painter.layout_no_wrap(msg.clone(), egui::FontId::monospace(11.0), theme::TX);
            let bg = egui::Rect::from_min_size(pos - egui::vec2(galley.size().x + 8.0, 0.0), galley.size() + egui::vec2(8.0, 2.0));
            painter.rect_filled(bg, egui::Rounding::same(3.0), egui::Color32::from_rgba_unmultiplied(20, 22, 26, 220));
            painter.galley(bg.min + egui::vec2(4.0, 1.0), galley, theme::TX);
        }
        if !self.toasts.is_empty() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }

        painter.text(
            rect.left_bottom() + egui::vec2(6.0, -4.0),
            egui::Align2::LEFT_BOTTOM,
            "LMB look · MMB pan · Alt+MMB orbit · wheel FOV · WASD fly · click/Shift/Alt/Ctrl select · Shift+drag box · drag handle move · ? shortcuts",
            egui::FontId::monospace(10.0),
            theme::FAINT,
        );
        action
    }
}

fn label_pos(vp: &DMat4, rect: egui::Rect, p: DVec3) -> egui::Pos2 {
    View3d::project(vp, rect, p + DVec3::new(0.0, 0.7, 0.0)).unwrap_or(rect.center()) + egui::vec2(6.0, 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sab_formats::editnodes::{Entry, Pack, Root};
    use sab_tools::stage::EditNodesSources;
    use std::path::Path;

    fn object(tag: u32, pos: [f32; 3]) -> Node {
        Node::Container {
            tag,
            children: vec![
                Node::Leaf { tag: tags::CLASS_NAME, data: b"Locator\0".to_vec() },
                Node::Leaf { tag: tags::POSITION, data: pos.iter().flat_map(|v| v.to_le_bytes()).collect() },
            ],
        }
    }

    /// Container 0x10 holds the primary (0xA) and a neighbour (0xB) 4 m away; container 0x20 holds
    /// a neighbour (0xC) from another container, and 0xD far outside the radius.
    fn session() -> Session {
        let base = Pack {
            entries: vec![
                Entry { hash: 0x10, root: Root::EditNode { objects: vec![object(0xA, [100.0, 10.0, 200.0]), object(0xB, [104.0, 10.0, 200.0])] } },
                Entry { hash: 0x20, root: Root::EditNode { objects: vec![object(0xC, [100.0, 10.0, 205.0]), object(0xD, [5000.0, 0.0, 0.0])] } },
            ],
        };
        Session::from_sources(Path::new("G"), EditNodesSources::from_bytes(base.write(), Pack { entries: vec![] }.write(), "LF".into()).unwrap())
    }

    #[test]
    fn edited_cell_replaces_its_original_context_without_hiding_neighbours() {
        let mut view = View3d::new();
        view.target = DVec3::new(100.0, 10.0, 200.0);
        let prop = |cell| crate::cell_props::Prop {
            pos: [100.0, 10.0, 200.0], axes: None, class_name: "building".into(), type_name: String::new(), cell, layer: 0, list: 0, tag: 1,
        };
        view.props = Loading::Ready(crate::cell_props::CellProps::from_props(vec![prop(1), prop(2)]));
        let session = session();
        view.refresh_markers(&session, &[]);
        assert_eq!(view.props_near.len(), 2);
        view.set_edit_cell(Some(1));
        view.refresh_markers(&session, &[]);
        assert_eq!(view.props_near, vec![1]);
        assert_eq!(view.markers.len(), 3);
        view.set_edit_cell(None);
        view.refresh_markers(&session, &[]);
        assert_eq!(view.props_near.len(), 2);
    }

    struct Rig {
        ctx: egui::Context,
        v: View3d,
        s: Session,
    }

    impl Rig {
        fn new() -> Rig {
            let ctx = egui::Context::default();
            theme::install(&ctx);
            let mut r = Rig { ctx, v: View3d::new(), s: session() };
            r.frame(&[(0x10, 0)], vec![], Default::default());
            r
        }

        fn frame(&mut self, sel: &[ObjRef], events: Vec<egui::Event>, modifiers: egui::Modifiers) -> Option<ViewAction> {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))), events, modifiers, ..Default::default() };
            let mut out = None;
            let (v, s) = (&mut self.v, &self.s);
            let _ = self.ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| out = v.show(ui, s, sel.first().copied(), sel, 500.0));
            });
            out
        }

        fn screen(&self, p: [f32; 3]) -> egui::Pos2 {
            let (rect, vp) = self.v.last.unwrap();
            View3d::project(&vp, rect, v3(p)).unwrap()
        }

        /// Move, press, release at one point: a click.
        fn click(&mut self, sel: &[ObjRef], p: egui::Pos2, m: egui::Modifiers) -> Option<ViewAction> {
            self.frame(sel, vec![egui::Event::PointerMoved(p)], m);
            self.frame(sel, vec![button(p, true, m)], m);
            self.frame(sel, vec![button(p, false, m)], m)
        }

        /// Press at `a`, move in steps to `b`, release at `b`; returns the action on release.
        fn drag(&mut self, sel: &[ObjRef], a: egui::Pos2, b: egui::Pos2, m: egui::Modifiers) -> Option<ViewAction> {
            self.frame(sel, vec![egui::Event::PointerMoved(a)], m);
            assert!(self.frame(sel, vec![button(a, true, m)], m).is_none());
            for k in 1..=4 {
                let r = self.frame(sel, vec![egui::Event::PointerMoved(a + (b - a) * (k as f32 / 4.0))], m);
                assert!(r.is_none(), "nothing is committed while dragging");
            }
            self.frame(sel, vec![button(b, false, m)], m)
        }
    }

    fn button(pos: egui::Pos2, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers }
    }

    const SHIFT: egui::Modifiers = egui::Modifiers { alt: false, ctrl: false, shift: true, mac_cmd: false, command: false };
    const ALT: egui::Modifiers = egui::Modifiers { alt: true, ctrl: false, shift: false, mac_cmd: false, command: false };

    #[test]
    fn double_clicking_a_cell_origin_requests_its_exact_placement_without_clearing_on_first_click() {
        let mut rig = Rig::new();
        let prop = crate::cell_props::Prop {
            pos: [102.0, 10.0, 202.0], axes: None, class_name: "city1_test".into(), type_name: String::new(), cell: 1700865, list: 3, tag: 0x1234, layer: 0x5678,
        };
        let target = prop.target();
        rig.v.props = Loading::Ready(crate::cell_props::CellProps::from_props(vec![prop]));
        rig.v.invalidate();
        let selected = [(0x10, 0)];
        rig.frame(&selected, vec![], Default::default());
        let origin = rig.screen([102.0, 10.4, 202.0]);
        assert!(rig.click(&selected, origin, Default::default()).is_none());
        assert_eq!(rig.click(&selected, origin, Default::default()), Some(ViewAction::OpenCell(target)));
        assert!(!matches!(rig.click(&selected, origin, SHIFT), Some(ViewAction::OpenCell(_))));
        assert!(!matches!(rig.click(&selected, origin, ALT), Some(ViewAction::OpenCell(_))));
    }

    #[test]
    fn empty_selection_keeps_camera_and_can_pick_a_neighbour() {
        let mut rig = Rig::new();
        let target = rig.v.target;
        rig.v.last = None;
        rig.frame(&[], vec![], Default::default());
        assert!(rig.v.last.is_some());
        assert_eq!(rig.v.target, target);
        let point = rig.screen([104.0, 10.0, 200.0]);
        assert_eq!(rig.click(&[], point, Default::default()), Some(ViewAction::Pick(Some((0x10, 1)), PickMode::Replace)));
    }

    #[test]
    fn mesh_surface_picking_uses_triangle_depth_and_instance_transform() {
        let geometry = crate::place_mesh::Geom {
            tris: vec![[[-2.0, -2.0, 0.0], [2.0, -2.0, 0.0], [0.0, 2.0, 0.0]]],
            normals: vec![[0.0, 0.0, 1.0]], uvs: vec![[[0.0; 2]; 3]], ranges: vec![], provenance: String::new(),
        };
        let ray = (DVec3::new(0.6, 0.0, 10.0), -DVec3::Z);
        assert_eq!(mesh_hit(&geometry, DMat4::IDENTITY, ray.0, ray.1), Some(10.0));
        assert!(mesh_hit(&geometry, DMat4::IDENTITY, DVec3::new(8.0, 0.0, 10.0), ray.1).is_none());
        let near = DMat4::from_scale_rotation_translation(DVec3::splat(2.0), DQuat::IDENTITY, DVec3::new(0.0, 0.0, 4.0));
        let mut best = None;
        pick_surface(&mut best, Some(ray), &geometry, near, SurfacePick::Object((1, 2)), None);
        pick_surface(&mut best, Some(ray), &geometry, DMat4::IDENTITY, SurfacePick::Object((3, 4)), None);
        assert!(matches!(best, Some((distance, SurfacePick::Object((1, 2)))) if (distance - 6.0).abs() < 1e-8));
    }

    #[test]
    fn neighbours_across_containers_are_shown_and_clicks_select_with_modifiers() {
        let mut r = Rig::new();
        let shown: Vec<ObjRef> = r.v.markers.iter().map(|m| m.obj).collect();
        assert_eq!(shown, vec![(0x10, 0), (0x10, 1), (0x20, 0)], "the far object is outside the radius");
        let p = r.screen([100.0, 10.0, 205.0]);
        let sel = [(0x10, 0)];
        assert_eq!(r.click(&sel, p, Default::default()), Some(ViewAction::Pick(Some((0x20, 0)), PickMode::Replace)));
        assert_eq!(r.click(&sel, p, SHIFT), Some(ViewAction::Pick(Some((0x20, 0)), PickMode::Add)));
        assert_eq!(r.click(&sel, p, ALT), Some(ViewAction::Pick(Some((0x20, 0)), PickMode::Remove)));
        let (rect, _) = r.v.last.unwrap();
        let empty = rect.left_top() + egui::vec2(15.0, 15.0);
        assert_eq!(r.click(&sel, empty, Default::default()), Some(ViewAction::Pick(None, PickMode::Replace)), "a click on empty space clears");
    }

    #[test]
    fn shift_drag_draws_a_box_that_selects_the_markers_inside() {
        let mut r = Rig::new();
        let sel = [(0x10, 0)];
        let (a, b) = (r.screen([100.0, 10.0, 200.0]), r.screen([104.0, 10.0, 200.0]));
        let (lo, hi) = (a.min(b) - egui::vec2(20.0, 20.0), a.max(b) + egui::vec2(20.0, 20.0));
        let c = r.screen([100.0, 10.0, 205.0]);
        assert!(!egui::Rect::from_two_pos(lo, hi).contains(c), "the test box must leave 0xC outside");
        match r.drag(&sel, lo, hi, SHIFT) {
            Some(ViewAction::PickMany(mut hits, PickMode::Replace)) => {
                hits.sort();
                assert_eq!(hits, vec![(0x10, 0), (0x10, 1)]);
            }
            other => panic!("Shift+drag selects by box, got {other:?}"),
        }
    }

    #[test]
    fn a_plain_left_drag_looks_around_without_editing() {
        let mut r = Rig::new();
        let sel = [(0x10, 0)];
        let (rect, _) = r.v.last.unwrap();
        let eye = r.v.eye();
        let (yaw, target) = (r.v.yaw, r.v.target);
        let a = rect.left_top() + egui::vec2(30.0, 200.0);
        assert!(r.drag(&sel, a, a + egui::vec2(80.0, 0.0), Default::default()).is_none());
        assert!(r.v.yaw != yaw && r.v.target != target, "the view turned");
        assert!(r.v.eye().distance(eye) < 1e-6, "looking turns in place: the eye stays put");
    }

    /// Dragging the gizmo's centre (view-plane translate) with two objects selected previews and asks
    /// for exactly one move of both, on release.
    #[test]
    fn one_gizmo_drag_moves_every_selected_object_once_on_release() {
        let mut r = Rig::new();
        let sel = [(0x10, 0), (0x10, 1)];
        r.frame(&sel, vec![], Default::default());
        // With two targets the gizmo sits at their mean.
        let p = r.screen([102.0, 10.0, 200.0]);
        let q = p + egui::vec2(60.0, -30.0);
        match r.drag(&sel, p, q, Default::default()) {
            Some(ViewAction::Move(moves)) => {
                assert_eq!(moves.len(), 2);
                let d0 = v3(moves[0].2) - DVec3::new(100.0, 10.0, 200.0);
                let d1 = v3(moves[1].2) - DVec3::new(104.0, 10.0, 200.0);
                assert!(d0.length() > 0.5 && d0.distance(d1) < 1e-3, "both moved by the same offset: {d0:?} {d1:?}");
            }
            other => panic!("releasing the drag commits one move, got {other:?}"),
        }
        assert!(r.frame(&sel, vec![], Default::default()).is_none());
        assert!(r.v.preview.is_empty());
    }

    /// Find a press point on the rotate gizmo's rings by scanning around the gizmo centre (the ring radius is the gizmo's own
    /// business); returns the committed action of the first drag that produces one.
    fn rotate_drag(r: &mut Rig, sel: &[ObjRef], centre: egui::Pos2) -> Option<(egui::Pos2, ViewAction)> {
        for radius in [40.0f32, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0] {
            for deg in (0..360).step_by(15) {
                let a = deg as f32 * std::f32::consts::PI / 180.0;
                let dir = egui::vec2(a.cos(), a.sin());
                let start = centre + dir * radius;
                let tangent = egui::vec2(-dir.y, dir.x) * 45.0;
                r.frame(sel, vec![egui::Event::PointerMoved(centre + egui::vec2(300.0, 200.0))], Default::default());
                if let Some(act) = r.drag(sel, start, start + tangent, Default::default()) {
                    if matches!(act, ViewAction::Rotate(..)) {
                        return Some((start, act));
                    }
                }
            }
        }
        None
    }

    /// Real viewport input path (egui pointer events through `View3d::show`): in Rotate mode a press-drag-release on the gizmo's ring
    /// commits exactly one Rotate on release (nothing while dragging), the matrix is a proper rotation, and applying it to the
    /// session is ONE undo step that restores the exact bytes. This is egui-level injection, not OS mouse input in a live window.
    #[test]
    fn a_rotate_gizmo_drag_commits_one_rotation_and_one_undo_restores_the_bytes() {
        let mut r = Rig::new();
        let sel = [(0x10, 0)];
        r.v.rotate_mode = true;
        r.frame(&sel, vec![], Default::default());
        let centre = r.screen([100.0, 10.0, 200.0]);
        let (_, act) = rotate_drag(&mut r, &sel, centre).expect("some drag on the rotate gizmo's rings must commit a Rotate");
        let ViewAction::Rotate(objs, m, _pivot) = act else { unreachable!() };
        assert_eq!(objs, vec![(0x10, 0)]);
        let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0]) + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
        assert!((det - 1.0).abs() < 1e-4, "a proper rotation (det {det}): {m:?}");
        assert!((0..3).any(|i| (m[i][i] - 1.0).abs() > 1e-3), "not the identity: {m:?}");
        assert!(r.frame(&sel, vec![], Default::default()).is_none(), "nothing further is committed");
        assert!(r.v.preview_rot.is_none() && r.v.preview.is_empty(), "the drag preview is cleared");

        // apply exactly as the page does (Session::rotate_objects) to an object that has a basis, then undo
        let mut node = object(0xA, [100.0, 10.0, 200.0]);
        if let Node::Container { children, .. } = &mut node {
            for (tag, v) in [(tags::X_AXIS, [1.0f32, 0.0, 0.0]), (tags::Y_AXIS, [0.0, 1.0, 0.0]), (tags::Z_AXIS, [0.0, 0.0, 1.0])] {
                children.push(Node::Leaf { tag, data: v.iter().flat_map(|x| x.to_le_bytes()).collect() });
            }
        }
        let base = Pack { entries: vec![Entry { hash: 0x10, root: Root::EditNode { objects: vec![node] } }] };
        let mut s = Session::from_sources(Path::new("G"), EditNodesSources::from_bytes(base.write(), Pack { entries: vec![] }.write(), "LF".into()).unwrap());
        let before = s.entry(0x10).unwrap().objects()[0].clone();
        assert_eq!(s.rotate_objects(&objs, m, None).unwrap(), 1);
        assert_ne!(s.entry(0x10).unwrap().objects()[0], before, "the rotation changed the basis");
        assert!(!s.active().is_empty(), "the rotation is recorded in the journal");
        assert!(s.undo().unwrap(), "one undo step");
        assert_eq!(s.entry(0x10).unwrap().objects()[0], before, "undo restores the exact bytes");
        assert!(s.active().is_empty(), "nothing left in the journal");
    }

    /// Characterisation (what the code does today, not what an editor might prefer): a gizmo drag has NO cancel path. Escape and a
    /// right-click during the drag do not abort it, so releasing still commits; undo is the recovery. (The G/R mouse modes do cancel
    /// on Escape or right-click, see `grab_with_the_mouse_commits_one_move_and_escape_cancels`.)
    #[test]
    fn escape_during_a_gizmo_drag_does_not_cancel_it_so_release_still_commits() {
        let mut r = Rig::new();
        let sel = [(0x10, 0), (0x10, 1)];
        r.frame(&sel, vec![], Default::default());
        let a = r.screen([102.0, 10.0, 200.0]);
        let b = a + egui::vec2(60.0, -30.0);
        r.frame(&sel, vec![egui::Event::PointerMoved(a)], Default::default());
        assert!(r.frame(&sel, vec![button(a, true, Default::default())], Default::default()).is_none());
        r.frame(&sel, vec![egui::Event::PointerMoved(a + (b - a) * 0.5)], Default::default());
        r.frame(&sel, vec![key(egui::Key::Escape)], Default::default());
        r.frame(&sel, vec![egui::Event::PointerMoved(b)], Default::default());
        let act = r.frame(&sel, vec![button(b, false, Default::default())], Default::default());
        assert!(matches!(act, Some(ViewAction::Move(_))), "release commits despite Escape (no gizmo cancel): {act:?}");
    }

    /// The world ray under the cursor (used for terrain picking) passes through the 3D point drawn at that pixel, and is None outside the view.
    #[test]
    fn hover_ray_passes_through_the_point_under_the_cursor() {
        let mut r = Rig::new();
        r.frame(&[(0x10, 0)], vec![], Default::default());
        for p in [[100.0, 10.0, 200.0], [104.0, 10.0, 200.0], [100.0, 10.0, 205.0]] {
            let px = r.screen(p);
            let (o, d) = r.v.hover_ray(px).expect("inside the view");
            assert!((d.length() - 1.0).abs() < 1e-9, "unit direction");
            let w = DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64);
            let along = (w - o).dot(d);
            let miss = (o + d * along - w).length();
            assert!(along > 0.0 && miss < 1e-3 * along.max(1.0), "the ray passes through {p:?} (miss {miss} m at {along} m)");
        }
        assert!(r.v.hover_ray(egui::pos2(-50.0, -50.0)).is_none(), "outside the view");
    }

    #[test]
    fn a_nudge_key_over_the_view_moves_the_whole_selection_once() {
        let mut r = Rig::new();
        let sel = [(0x10, 0), (0x20, 0)];
        let (rect, _) = r.v.last.unwrap();
        let corner = rect.left_top() + egui::vec2(20.0, 20.0);
        r.frame(&sel, vec![egui::Event::PointerMoved(corner)], Default::default());
        let key = egui::Event::Key { key: egui::Key::ArrowRight, physical_key: None, pressed: true, repeat: false, modifiers: SHIFT };
        assert_eq!(r.frame(&sel, vec![key], SHIFT), Some(ViewAction::Move(vec![(0x10, 0, [101.0, 10.0, 200.0]), (0x20, 0, [101.0, 10.0, 205.0])])));
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }
    }

    /// G starts a mouse grab, pointer motion previews without committing, and a click commits exactly
    /// one move. Esc cancels without an action.
    #[test]
    fn grab_with_the_mouse_commits_one_move_and_escape_cancels() {
        let mut r = Rig::new();
        let sel = [(0x10, 0)];
        let start = r.screen([100.0, 10.0, 200.0]);
        r.frame(&sel, vec![egui::Event::PointerMoved(start)], Default::default());
        assert!(r.frame(&sel, vec![key(egui::Key::G)], Default::default()).is_none());
        assert!(r.v.modal_active(), "G enters the grab");
        let q = start + egui::vec2(70.0, -25.0);
        assert!(r.frame(&sel, vec![egui::Event::PointerMoved(q)], Default::default()).is_none(), "moving previews, never commits");
        assert!(r.frame(&sel, vec![button(q, true, Default::default())], Default::default()).is_none());
        match r.frame(&sel, vec![button(q, false, Default::default())], Default::default()) {
            Some(ViewAction::Move(moves)) => {
                assert_eq!(moves.len(), 1);
                assert_eq!(moves[0].0, 0x10);
                assert!(v3(moves[0].2).distance(DVec3::new(100.0, 10.0, 200.0)) > 0.1, "the object moved: {:?}", moves[0].2);
            }
            other => panic!("click commits one Move, got {other:?}"),
        }
        assert!(!r.v.modal_active());

        // A second grab, cancelled with Esc: no action, no preview left behind.
        r.frame(&sel, vec![egui::Event::PointerMoved(start)], Default::default());
        r.frame(&sel, vec![key(egui::Key::G)], Default::default());
        assert!(r.v.modal_active());
        assert!(r.frame(&sel, vec![key(egui::Key::Escape)], Default::default()).is_none());
        assert!(!r.v.modal_active() && r.v.preview.is_empty());
    }

    /// X/Y/Z constrain a grab to one world axis.
    #[test]
    fn an_axis_key_constrains_a_grab_to_that_axis() {
        let mut r = Rig::new();
        let sel = [(0x10, 0)];
        let start = r.screen([100.0, 10.0, 200.0]);
        r.frame(&sel, vec![egui::Event::PointerMoved(start)], Default::default());
        r.frame(&sel, vec![key(egui::Key::G)], Default::default());
        r.frame(&sel, vec![key(egui::Key::X)], Default::default());
        let q = start + egui::vec2(0.0, 90.0);
        r.frame(&sel, vec![egui::Event::PointerMoved(q)], Default::default());
        r.frame(&sel, vec![button(q, true, Default::default())], Default::default());
        match r.frame(&sel, vec![button(q, false, Default::default())], Default::default()) {
            Some(ViewAction::Move(moves)) => {
                let d = v3(moves[0].2) - DVec3::new(100.0, 10.0, 200.0);
                assert!(d.x != 0.0 && d.y.abs() < 1e-4 && d.z.abs() < 1e-4, "X-constrained: {d:?}");
            }
            other => panic!("X-constrained grab commits a Move, got {other:?}"),
        }
    }

    /// R starts a mouse rotation about Y; a click commits one rotation (a matrix, about the pivot).
    #[test]
    fn rotate_with_the_mouse_commits_one_rotation() {
        let mut r = Rig::new();
        let sel = [(0x10, 0)];
        let pivot = r.screen([100.0, 10.0, 200.0]);
        let p = pivot + egui::vec2(-70.0, 0.0);
        r.frame(&sel, vec![egui::Event::PointerMoved(p)], Default::default());
        assert!(r.frame(&sel, vec![key(egui::Key::R)], Default::default()).is_none());
        assert!(r.v.modal_active(), "R enters the rotate");
        let q = pivot + egui::vec2(0.0, -70.0);
        r.frame(&sel, vec![egui::Event::PointerMoved(q)], Default::default());
        r.frame(&sel, vec![button(q, true, Default::default())], Default::default());
        match r.frame(&sel, vec![button(q, false, Default::default())], Default::default()) {
            Some(ViewAction::Rotate(objs, r, _pivot)) => {
                assert_eq!(objs, vec![(0x10, 0)]);
                // XAxis turns away from +X (a yaw): the X row is not the identity.
                assert!((r[0][0] - 1.0).abs() > 1e-3, "rotation matrix {r:?}");
            }
            other => panic!("click commits one Rotate, got {other:?}"),
        }
        assert!(!r.v.modal_active());
    }

    /// A room of one big wall facing the eye, `dist` metres along the eye → (104,10,200) line.
    fn wall_at(r: &mut Rig, dist: f64) {
        use crate::place_mesh::Geom;
        use crate::place_world::{Object, Piece, World};
        let eye = r.v.eye();
        let b = DVec3::new(104.0, 10.0, 200.0);
        let dir = (b - eye).normalize();
        let c = eye + dir * dist;
        let up = if dir.y.abs() > 0.9 { DVec3::X } else { DVec3::Y };
        let (u, v) = (dir.cross(up).normalize() * 40.0, dir.cross(dir.cross(up).normalize()).normalize() * 40.0);
        let q = |a: f64, b: f64| f3(c + u * a + v * b);
        let tris = vec![[q(-1.0, -1.0), q(1.0, -1.0), q(-1.0, 1.0)], [q(1.0, -1.0), q(1.0, 1.0), q(-1.0, 1.0)]];
        let geom = std::sync::Arc::new(Geom {
            normals: vec![[0.0, 1.0, 0.0]; 2],
            uvs: vec![[[0.0; 2]; 3]; 2],
            ranges: vec![crate::place_mesh::GeomRange { first: 0, count: 2, material: 0 }],
            tris,
            provenance: "test wall".into(),
        });
        let identity = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
        let piece = Piece { key: "wall".into(), geom, model: identity };
        let obj = Object {
            name: "test".into(),
            hash: 1,
            pack: "test".into(),
            crc: 1,
            a: [0.0; 3],
            b: [0.0; 3],
            pieces: vec![piece],
            not_drawn: 0,
            notes: Vec::new(),
            tex: std::sync::Arc::new(crate::place_tex::TextureSet::empty("")),
        };
        r.v.world = World::Ready([0.0; 3], vec![obj]);
    }

    /// Objects the room hides can't be clicked or box-selected unless "through walls" is on, and an
    /// object in front of the wall is unaffected.
    #[test]
    fn picking_respects_the_rooms_walls_unless_through_walls_is_on() {
        let sel = [(0x10, 0)];
        for (dist, hidden) in [(3.0, true), (60.0, false)] {
            let mut r = Rig::new();
            wall_at(&mut r, dist);
            r.frame(&sel, vec![], Default::default());
            let b = r.screen([104.0, 10.0, 200.0]);
            let want = Some(ViewAction::Pick(Some((0x10, 1)), PickMode::Replace));
            let got = r.click(&sel, b, Default::default());
            if hidden {
                assert_eq!(got, Some(ViewAction::Pick(None, PickMode::Replace)), "behind the wall: the click selects nothing");
                r.v.through_walls = true;
                r.frame(&sel, vec![], Default::default());
                assert_eq!(r.click(&sel, b, Default::default()), want, "through walls it can be picked");
            } else {
                assert_eq!(got, want, "a wall behind the object doesn't hide it");
            }
            // A box around B and the primary.
            r.v.through_walls = false;
            r.frame(&sel, vec![], Default::default());
            let (a, b2) = (r.screen([100.0, 10.0, 200.0]), r.screen([104.0, 10.0, 200.0]));
            let (lo, hi) = (a.min(b2) - egui::vec2(20.0, 20.0), a.max(b2) + egui::vec2(20.0, 20.0));
            let Some(ViewAction::PickMany(hits, _)) = r.drag(&sel, lo, hi, SHIFT) else { panic!("a box selects") };
            assert_eq!(hits.contains(&(0x10, 1)), !hidden, "box select skips hidden objects (hidden = {hidden}): {hits:?}");
        }
    }

    #[test]
    fn the_wheel_changes_the_field_of_view_and_alt_middle_click_resets_it() {
        let mut r = Rig::new();
        let sel = [(0x10, 0)];
        let (rect, _) = r.v.last.unwrap();
        let p = rect.center() + egui::vec2(200.0, 100.0);
        r.frame(&sel, vec![egui::Event::PointerMoved(p)], Default::default());
        r.frame(&sel, vec![egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, 200.0), modifiers: Default::default() }], Default::default());
        assert!(r.v.fov < FOV_DEFAULT - 5.0, "wheel up narrows the view: {}", r.v.fov);
        let mid = |pressed| egui::Event::PointerButton { pos: p, button: egui::PointerButton::Middle, pressed, modifiers: ALT };
        r.frame(&sel, vec![mid(true)], ALT);
        r.frame(&sel, vec![mid(false)], ALT);
        assert_eq!(r.v.fov, FOV_DEFAULT);
    }
}
