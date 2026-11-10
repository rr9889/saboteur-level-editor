//! The "places" page: browse the effective EditNodes data (DLC slot first, then base), inspect an
//! object's parameters, move it, undo/redo, save/reopen a project and stage a DLC/01 override.
//!
//! Presentation only: the session, journal, project file and staging live in `sab_tools`
//! (`session`, `editnodes_stage`), and the byte formats in `sab_formats`. Staging writes to a folder
//! under the export directory, never into the game folder; installing is the user's step.

use std::path::{Path, PathBuf};

use sab_formats::editnodes::{self, tags, LuaValue, Node, Value};
use sab_tools::areas::{self, Area, Kind};
use sab_tools::editnodes_stage::Edit;
use sab_tools::livelink::{LiveLink, QueryObject, Status as LiveStatus};
use sab_tools::session::{ContainerInfo, Session, Severity};

use crate::gui::theme;
use crate::placements_view::{ObjRef, PickMode, View3d, ViewAction};

fn cell_target_object(session: &Session, container: u32, layer: u32, target: &crate::cell_props::CellTarget) -> Result<ObjRef, String> {
    let entry = session.entry(container).ok_or("placement list no longer exists")?;
    let matches: Vec<_> = entry.objects().iter().enumerate().filter(|(_, object)| object.tag() == target.tag).collect();
    if matches.len() != 1 {
        return Err(format!("placement {:08x} is missing or ambiguous in list {}; restart Workshop to refresh cell props", target.tag, target.list));
    }
    let (index, object) = matches[0];
    if layer != target.layer || object.class_name().unwrap_or_default() != target.class_name || object.position() != Some(target.pos) {
        return Err("placement changed since the viewport loaded it; restart Workshop to refresh cell props before editing".into());
    }
    Ok((container, index))
}

/// Every binding on the page: (group, keys, action, origin). "Ariane" marks bindings that follow
/// Ariane's control scheme (`docs/level_editor/_audit/group_C_ariane.md`); "places" marks this
/// editor's own.
const SHORTCUTS: &[(&str, &str, &str, &str)] = &[
    ("Camera", "LMB drag", "Look around (turn in place)", "Ariane"),
    ("Camera", "Ctrl+Alt+LMB drag", "Dolly along the view direction", "Ariane"),
    ("Camera", "MMB drag", "Pan", "Ariane"),
    ("Camera", "Alt+MMB drag", "Orbit around the camera target", "Ariane"),
    ("Camera", "Ctrl+Alt+MMB drag", "Zoom toward the target", "Ariane"),
    ("Camera", "Mouse wheel", "Field of view", "Ariane"),
    ("Camera", "Alt+MMB click", "Reset the field of view to 70°", "Ariane"),
    ("Camera", "W / S, A / D", "Fly forward / back, left / right", "Ariane"),
    ("Camera", "Shift / Alt + WASD", "Fly fast / slow", "Ariane"),
    ("Camera", "RMB drag", "Orbit (kept from the first version)", "places"),
    ("Camera", "Focus button", "Centre on the primary selection", "places"),
    ("Selection", "Click object", "Select", "Ariane"),
    ("Selection", "Shift+click", "Add to the selection", "Ariane"),
    ("Selection", "Alt+click", "Remove from the selection", "Ariane"),
    ("Selection", "Ctrl+click", "Toggle in the selection", "Ariane"),
    ("Selection", "Click empty space", "Clear the selection", "Ariane"),
    ("Selection", "Shift+LMB drag", "Box select", "Ariane"),
    ("Selection", "Ctrl+Shift / Alt+Shift + LMB drag", "Box add / box remove", "Ariane"),
    ("Selection", "Esc", "Clear the selection", "places"),
    ("Gizmo", "Drag a handle", "Move the selection; one undo step per drag", "Ariane"),
    ("Gizmo", "Shift while dragging", "Snap to the chosen increment", "Ariane"),
    ("Gizmo", "L", "Toggle local / world axes", "Ariane"),
    ("Gizmo", "W", "Translate mode", "Ariane"),
    ("Gizmo", "Q", "Rotate mode: composes on the stored basis, never Euler sums (a 90° yaw was demonstrated in game on a locator; tilts were not)", "Ariane / CP77 lesson"),
    ("Gizmo", "Shift while rotating", "Snap to 15°", "Ariane"),
    ("Move", "Arrows, PgUp / PgDn (over the view)", "Nudge 0.1 m (Shift 1 m), one undo step", "places"),
    ("Move", "Apply move / Apply offset", "Typed position / offset", "places"),
    ("Transform", "G (over the view)", "Grab/move the selection with the mouse; X/Y/Z constrain an axis; Shift snaps; click or Enter applies, Esc cancels", "Ariane"),
    ("Transform", "R (over the view)", "Rotate the selection with the mouse about Y (X/Y/Z choose the axis); Shift snaps to 15°; click or Enter applies, Esc cancels", "Ariane"),
    ("Transform", "X / Y / Z during G or R", "Constrain to that world axis (press again to release)", "places"),
    ("Transform", "Shift+G", "Snap/align to ground: not available (no ground data)", "Ariane"),
    ("Edit", "Ctrl+Alt+V", "Paste special: camera target or in place, extra offset, keep a cut's tags", "places"),
    ("Edit", "Change ClassName… (inspector)", "Pick a class observed on this install or type a GameTemplates name; one undo step", "places"),
    ("Edit", "Ctrl+C / Ctrl+X", "Copy / cut the selection", "Ariane"),
    ("Edit", "Ctrl+V / Ctrl+Shift+V", "Paste at the camera target / in place; copies get name_N tags", "Ariane / CP77"),
    ("Edit", "Ctrl+D", "Duplicate one snap step along X", "places"),
    ("Edit", "Delete / Backspace", "Delete the selection (one undo step)", "Ariane"),
    ("Edit", "Ctrl+Shift+E / Ctrl+Shift+I", "Export / import a prefab (JSON, relative positions)", "Ariane"),
    ("Edit", "LuaParam fields", "Typed edit by occurrence; a resize is flagged", "places / W3 raw report"),
    ("File", "Ctrl+Z / Ctrl+Y", "Undo / redo a named step; the step's objects are selected again", "Ariane / CP77 / Hammer"),
    ("File", "Ctrl+S", "Save project (asks for a file the first time)", "Ariane"),
    ("File", "Ctrl+O", "Open project", "Ariane"),
    ("File", "Stage button", "Stage the DLC/01 override (never into the game)", "places"),
    ("Windows", "B", "Toggle the container / object browser", "Ariane"),
    ("Windows", "I", "Toggle the object info (parameters)", "Ariane"),
    ("Windows", "F", "Toggle the changes panel", "Ariane"),
    ("Windows", "Click a change", "Select that step's objects and jump to them", "Ariane"),
    ("Windows", "Checks panel", "Errors block staging; click to go to the object", "Hammer / CP77"),
    ("Windows", "?", "This list", "places"),
];

/// The area browser's type filter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AreaFilter {
    All,
    Interiors,
    Exteriors,
}

impl AreaFilter {
    const ALL: [AreaFilter; 3] = [AreaFilter::All, AreaFilter::Interiors, AreaFilter::Exteriors];

    fn label(self) -> &'static str {
        match self {
            AreaFilter::All => "All",
            AreaFilter::Interiors => "Interiors",
            AreaFilter::Exteriors => "Exteriors",
        }
    }

    /// Technical map objects are neither: they appear under All only.
    fn admits(self, k: Kind) -> bool {
        matches!((self, k), (AreaFilter::All, _) | (AreaFilter::Interiors, Kind::Interior) | (AreaFilter::Exteriors, Kind::Exterior))
    }
}

pub struct Places {
    session: Option<Session>,
    tried_open: bool,
    containers: Vec<ContainerInfo>,
    filter: String,
    /// The primary selection (shown in the inspector) is `sel_container`/`sel_object`; `extra`
    /// holds the other selected objects.
    sel_container: Option<u32>,
    sel_object: Option<usize>,
    extra: Vec<ObjRef>,
    show_browser: bool,
    /// The area browser: built from the open session, rebuilt when the session's revision changes.
    /// An `Arc` so the draw code can hold the list while it changes selection state.
    areas: Option<Result<std::sync::Arc<Vec<Area>>, String>>,
    areas_rev: u64,
    /// Wall-clock milliseconds the last area-list build took, and how many builds there were.
    areas_ms: f64,
    areas_builds: u32,
    /// The area whose object list is open (toolbar breadcrumb and its resource panel).
    current_area: Option<usize>,
    /// The observed per-class parameter schema, rebuilt with the areas.
    schema: Option<crate::placement_schema::Schema>,
    schema_rev: u64,
    /// ClassName picker: `Some(filter)` while its window is open.
    class_picker: Option<String>,
    /// Paste Special's choices while its dialog is open.
    paste_special: Option<PasteSpecial>,
    /// The last staged folder, for "open folder" and the install command.
    staged_dir: Option<PathBuf>,
    /// B1: the live readback over the sab_asi `sab_cmd.lua` bridge.
    live: LiveLink,
    /// Selected objects the last readback could not address because no Lua path is recovered.
    live_missing: usize,
    area_kind: AreaFilter,
    area_filter: String,
    /// Read-only terrain index (loaded on demand) and the last terrain hit under the cursor.
    terrain: crate::place_terrain::TerrainLink,
    terrain_hover: Option<String>,
    /// Terrain PREVIEW (never saved or exported): show flag, the sampled patch with its sculpt offsets, brush settings, the last terrain point under the cursor,
    /// and a serial that changes with the geometry (the GPU cache key).
    terrain_on: bool,
    terrain_patch: Option<crate::place_terrain::Patch>,
    terrain_serial: u64,
    terrain_sent: u64,
    terrain_brush_radius: f32,
    terrain_brush_strength: f32,
    terrain_hit: Option<[f64; 3]>,
    /// `SAB_WORKSHOP_TERRAIN_DEMO=1`: load the terrain, show it and raise the demo hills once, with no clicks (for demos and screenshots).
    terrain_demo_pending: bool,
    /// EXPERIMENTAL terrain-cell editing (feature flag: env SAB_WORKSHOP_TERRAIN_CELLS). `cell_edit` is Some only while a cell session is open.
    cells_enabled: bool,
    cell_input: String,
    cell_edit: Option<sab_tools::cellsession::CellEdit>,
    /// Go to Object: the query and its cached results (searched only when the query changes).
    goto_query: String,
    goto_cache: Option<(String, Vec<sab_tools::areas::ObjectHit>, usize)>,
    area_sel: Option<usize>,
    show_changes: bool,
    show_info: bool,
    /// Left tab: Areas (true) or Containers. Right tab: Inspector (true) or Changes.
    nav_areas: bool,
    right_inspector: bool,
    /// The session revision at the last open or save; a different revision means unsaved edits.
    saved_rev: u64,
    export_dir: String,
    /// Set by the "Choose export folder…" button; the app takes it and runs its folder picker.
    want_export_dir: bool,
    show_shortcuts: bool,
    shortcut_filter: String,
    /// Copied objects with their source containers; `clip_cut` when they came from a cut (a cut's
    /// paste keeps the original tags while they are free).
    clipboard: Vec<(u32, Node)>,
    clip_cut: bool,
    /// Text being typed into a string or crc LuaParam field: (container, object, occurrence).
    lua_buf: std::collections::HashMap<(u32, usize, usize), String>,
    /// The position fields being edited, and the object they were filled from.
    pos_edit: [f32; 3],
    pos_for: Option<(u32, usize, Vec<u8>)>,
    /// The typed offset for "Apply offset", in metres.
    off_edit: [f32; 3],
    status: Option<(String, bool)>,
    report: Option<String>,
    project: Option<PathBuf>,
    /// An object to select once the data is open: (container, object tag).
    pending_select: Option<(u32, u32)>,
    autotest: Option<Autotest>,
    /// TEST ONLY: the path a file dialog would return (a native dialog would block a test), and the last rect of the toolbar buttons the GUI test clicks.
    #[cfg(test)]
    test_dialog: Option<PathBuf>,
    #[cfg(test)]
    test_discard_answer: Option<bool>,
    #[cfg(test)]
    test_rects: std::collections::HashMap<&'static str, egui::Rect>,
    view: View3d,
    show_view: bool,
    /// The game folder the page was last shown with (the 3D view's cell props load from it).
    game_dir: String,
}

struct Autotest {
    dir: PathBuf,
    step: usize,
    next: std::time::Instant,
    log: Vec<String>,
    orig: Option<[f32; 3]>,
}

/// The Paste Special dialog's choices. Every paste is one undo step and never rewrites references.
struct PasteSpecial {
    /// Paste the clipboard's centre at the camera target (off: in place).
    at_target: bool,
    /// Keep the clipboard's own tags when they are free (a cut's paste); off: mint new tags.
    keep_tags: bool,
    /// Extra world-space offset applied after the camera/in-place placement, in metres.
    offset: [f32; 3],
}

fn show_value(v: Option<Value>) -> String {
    match v {
        None => "(container)".into(),
        Some(Value::Empty) => "(empty)".into(),
        Some(Value::Bool(b)) => format!("bool {b}"),
        Some(Value::U32(x)) => format!("0x{x:08x}  ({x}; f32 {})", f32::from_bits(x)),
        Some(Value::Raw64(b)) => format!("raw64 {}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        Some(Value::Str(s)) => format!("\"{s}\""),
        Some(Value::Vec3(p)) => format!("({:.3}, {:.3}, {:.3})", p[0], p[1], p[2]),
        Some(Value::Lua(l)) => {
            let val = match &l.value {
                LuaValue::Crc(h) => format!("crc 0x{h:08x}"),
                LuaValue::Bool(b) => format!("bool {b}"),
                LuaValue::Float(f) => format!("float {f}"),
                LuaValue::Int(i) => format!("int {i}"),
                LuaValue::Str(s) => format!("\"{s}\""),
                LuaValue::CrcList(v) => format!("list [{}]", v.join(", ")),
            };
            format!("Lua {} = {val}", l.name)
        }
        Some(Value::BadLua(e)) => format!("LuaParam (undecoded: {e})"),
        Some(Value::Bytes(n)) => format!("{n} bytes"),
    }
}

fn short(name: Option<&str>, hash: u32) -> String {
    match name {
        Some(n) => n.trim_start_matches(r"France\EditNodes\").to_string(),
        None => format!("0x{hash:08x}"),
    }
}

/// The game's Lua node name for an EditNodes container: the stored path without the
/// `France\EditNodes\` prefix and the `.wsd` suffix (`Util.GetEditNodeContents` appends `.wsd` itself).
fn lua_node_name(container: u32) -> Option<String> {
    let n = editnodes::tag_name(container)?;
    let n = n.strip_prefix(r"France\EditNodes\").unwrap_or(n);
    let n = n.strip_suffix(".wsd").unwrap_or(n);
    (!n.is_empty()).then(|| n.to_string())
}

/// One undo step as the changes panel shows it.
struct ChangeRow {
    name: String,
    edits: usize,
    active: bool,
    /// Every edit is of a kind demonstrated in game on locators (`Edit::kind_demonstrated_on_locators`).
    /// That is evidence about the operation, not a verification of this particular edit.
    demonstrated: bool,
    /// The step's objects that exist in the current state.
    objects: Vec<ObjRef>,
    detail: String,
}

fn demonstrated_kind(e: &Edit) -> bool {
    e.kind_demonstrated_on_locators()
}

fn change_rows(s: &Session) -> Vec<ChangeRow> {
    let (journal, cursor) = s.journal();
    let groups = s.groups();
    let mut rows: Vec<ChangeRow> = Vec::new();
    let mut k = 0;
    while k < journal.len() {
        let g = groups[k];
        let start = k;
        while k < journal.len() && groups[k] == g {
            k += 1;
        }
        let edits = &journal[start..k];
        let mut objects: Vec<ObjRef> = Vec::new();
        let mut lines = Vec::new();
        for e in edits {
            let obj = s.object_name(e.object_tag()).map(|n| n.rsplit('\\').next().unwrap_or(&n).to_string()).unwrap_or_else(|| format!("0x{:08x}", e.object_tag()));
            let what = match e {
                Edit::Param(p) => editnodes::tag_name(p.param_tag).unwrap_or("param").to_string(),
                Edit::Insert { .. } => "insert".into(),
                Edit::Remove { .. } => "remove".into(),
            };
            lines.push(format!("{what} · {obj} (0x{:08x} #{})", e.container(), e.object_index()));
            let r = (e.container(), e.object_index());
            let exists = s.entry(r.0).and_then(|en| en.objects().get(r.1)).map_or(false, |o| o.tag() == e.object_tag());
            if exists && !matches!(e, Edit::Remove { .. }) && !objects.contains(&r) {
                objects.push(r);
            }
        }
        if lines.len() > 12 {
            let more = lines.len() - 12;
            lines.truncate(12);
            lines.push(format!("… and {more} more"));
        }
        rows.push(ChangeRow {
            name: s.group_name(g).to_string(),
            edits: edits.len(),
            active: start < cursor,
            demonstrated: edits.iter().all(demonstrated_kind),
            objects,
            detail: lines.join("\n"),
        });
    }
    rows
}

fn stamp() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Places {
    pub fn new() -> Places {
        Places {
            session: None,
            tried_open: false,
            containers: Vec::new(),
            filter: String::new(),
            sel_container: None,
            sel_object: None,
            extra: Vec::new(),
            show_browser: true,
            areas: None,
            areas_rev: 0,
            areas_ms: 0.0,
            areas_builds: 0,
            current_area: None,
            schema: None,
            schema_rev: 0,
            class_picker: None,
            paste_special: None,
            staged_dir: None,
            live: LiveLink::new(),
            live_missing: 0,
            area_kind: AreaFilter::All,
            area_filter: String::new(),
            terrain: crate::place_terrain::TerrainLink::default(),
            terrain_hover: None,
            terrain_on: true,
            terrain_patch: None,
            terrain_serial: 0,
            terrain_sent: u64::MAX,
            terrain_brush_radius: 30.0,
            terrain_brush_strength: 6.0,
            terrain_hit: None,
            terrain_demo_pending: std::env::var_os("SAB_WORKSHOP_TERRAIN_DEMO").is_some(),
            cells_enabled: std::env::var_os("SAB_WORKSHOP_TERRAIN_CELLS").is_some(),
            cell_input: String::new(),
            cell_edit: None,
            goto_query: String::new(),
            goto_cache: None,
            area_sel: None,
            show_changes: true,
            show_info: true,
            nav_areas: true,
            right_inspector: true,
            saved_rev: 0,
            export_dir: String::new(),
            want_export_dir: false,
            show_shortcuts: false,
            shortcut_filter: String::new(),
            clipboard: Vec::new(),
            clip_cut: false,
            lua_buf: std::collections::HashMap::new(),
            pos_edit: [0.0; 3],
            pos_for: None,
            off_edit: [0.0; 3],
            status: None,
            report: None,
            project: None,
            pending_select: None,
            autotest: None,
            #[cfg(test)]
            test_dialog: None,
            #[cfg(test)]
            test_discard_answer: None,
            #[cfg(test)]
            test_rects: std::collections::HashMap::new(),
            view: View3d::new(),
            show_view: true,
            game_dir: String::new(),
        }
    }

    /// Select `(container, object tag)` as soon as the EditNodes data is open.
    pub fn preselect(&mut self, container: u32, tag: u32) {
        self.pending_select = Some((container, tag));
    }

    fn apply_pending_select(&mut self) {
        let (Some(s), Some((c, t))) = (self.session.as_ref(), self.pending_select) else { return };
        self.pending_select = None;
        match s.entry(c).and_then(|e| e.objects().iter().position(|o| o.tag() == t)) {
            Some(i) => {
                self.sel_container = Some(c);
                self.sel_object = Some(i);
                self.filter = format!("0x{c:08x}");
            }
            None => self.status = Some((format!("object {t:08x} not found in container {c:08x}"), true)),
        }
    }

    /// One line for the rail tooltip and status bar.
    pub fn status_line(&self) -> String {
        match &self.session {
            None => "EditNodes not opened".into(),
            Some(s) => {
                let (j, c) = s.journal();
                format!("{} containers · {c} active edits ({} in journal)", self.containers.len(), j.len())
            }
        }
    }

    fn primary(&self) -> Option<ObjRef> {
        Some((self.sel_container?, self.sel_object?))
    }

    /// Every selected object, the primary first.
    fn selected(&self) -> Vec<ObjRef> {
        self.primary().into_iter().chain(self.extra.iter().copied()).collect()
    }

    /// Apply a pick (from the view or the lists) to the selection.
    fn pick(&mut self, hit: Option<ObjRef>, mode: PickMode) {
        let mut sel = self.selected();
        match (hit, mode) {
            (None, PickMode::Replace) => sel.clear(),
            (None, _) => {}
            (Some(h), PickMode::Replace) => sel = vec![h],
            (Some(h), PickMode::Add) => {
                sel.retain(|x| *x != h);
                sel.insert(0, h);
            }
            (Some(h), PickMode::Remove) => sel.retain(|x| *x != h),
            (Some(h), PickMode::Toggle) => {
                if sel.contains(&h) {
                    sel.retain(|x| *x != h);
                } else {
                    sel.insert(0, h);
                }
            }
        }
        self.set_selection(sel);
    }

    fn pick_many(&mut self, hits: Vec<ObjRef>, mode: PickMode) {
        let mut sel = self.selected();
        match mode {
            // The current primary stays primary when the box includes it.
            PickMode::Replace => {
                let keep = self.primary().filter(|p| hits.contains(p));
                sel = keep.into_iter().chain(hits.into_iter().filter(|h| Some(*h) != keep)).collect();
            }
            PickMode::Add | PickMode::Toggle => {
                for h in hits {
                    if !sel.contains(&h) {
                        sel.push(h);
                    }
                }
            }
            PickMode::Remove => sel.retain(|x| !hits.contains(x)),
        }
        self.set_selection(sel);
    }

    /// The first entry becomes the primary selection.
    fn set_selection(&mut self, sel: Vec<ObjRef>) {
        let (c, o) = sel.first().map_or((None, None), |&(c, i)| (Some(c), Some(i)));
        if c != self.sel_container {
            self.filter.clear();
        }
        self.sel_container = c;
        self.sel_object = o;
        self.extra = sel.into_iter().skip(1).collect();
        self.pos_for = None;
    }

    /// True once, after the user asked to choose the export folder.
    pub fn take_export_dir_request(&mut self) -> bool {
        std::mem::take(&mut self.want_export_dir)
    }

    /// True when the session has edits (or undo/redo steps) since it was opened or last saved.
    pub fn is_dirty(&self) -> bool {
        self.session.as_ref().map_or(false, |s| s.revision() != self.saved_rev)
    }

    /// Ask before discarding unsaved edits; true when it is fine to go on.
    pub fn confirm_discard(&self, action: &str) -> bool {
        if !self.is_dirty() {
            return true;
        }
        #[cfg(test)]
        if let Some(answer) = self.test_discard_answer {
            return answer;
        }
        let n = self.session.as_ref().map_or(0, |s| s.journal().1);
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("Unsaved placement edits")
            .set_description(format!("Your placement edits changed since the last save ({n} active edit(s) now) and are not in a project file.\n\n{action}\n\nContinue and lose them?"))
            .set_buttons(rfd::MessageButtons::YesNo)
            .show()
            == rfd::MessageDialogResult::Yes
    }

    /// The game folder changed: drop everything read from the old install and reopen from the new one.
    pub fn switch_game(&mut self) {
        self.live.restore();
        self.cell_edit = None;
        self.session = None;
        self.containers.clear();
        self.areas = None;
        self.area_sel = None;
        self.sel_container = None;
        self.sel_object = None;
        self.extra.clear();
        self.project = None;
        self.report = None;
        self.pos_for = None;
        self.tried_open = false;
        self.saved_rev = 0;
        self.view.invalidate();
        self.status = Some(("game folder changed: reopening EditNodes".into(), false));
    }

    fn set_session(&mut self, s: Session) {
        self.cell_edit = None;
        self.project = None;
        self.staged_dir = None;
        self.report = None;
        self.sel_container = None;
        self.sel_object = None;
        self.current_area = None;
        self.goto_cache = None;
        self.schema = None;
        self.class_picker = None;
        self.paste_special = None;
        self.saved_rev = s.revision();
        self.containers = s.containers();
        self.areas = None;
        self.area_sel = None;
        self.extra.clear();
        self.session = Some(s);
        self.pos_for = None;
        self.view.invalidate();
    }

    /// EXPERIMENTAL: open one terrain cell as the editing session (flag-gated). Replaces the current session; refuses, with the reason, anything outside the verified scope.
    pub fn open_cell(&mut self, game_dir: &str, cell: u32) {
        if !self.cells_enabled {
            self.status = Some(("terrain-cell editing is experimental and off: set SAB_WORKSHOP_TERRAIN_CELLS=1 to enable it".into(), true));
            return;
        }
        if !self.confirm_discard("Opening a terrain cell replaces the current session.") {
            return;
        }
        match sab_tools::cellsession::open_cell(Path::new(game_dir), cell) {
            Ok((edit, s)) => {
                let n = s.containers().len();
                self.set_session(s);
                self.game_dir = game_dir.to_string();
                self.status = Some((format!("terrain cell {cell} open ({n} placement list(s), won by {}): move, rotate, duplicate or delete, then Stage mod writes a France patch megapack (experimental). Note: {}", edit.source_rel, sab_tools::cellsession::COLLISION_CAVEAT), false));
                self.cell_edit = Some(edit);
                self.focus_cell_objects();
            }
            Err(e) => self.status = Some((format!("terrain cell {cell} cannot be opened for editing: {e}"), true)),
        }
    }

    fn focus_cell_objects(&mut self) {
        let group = self.session.as_ref().and_then(|session| {
            let index = session.journal().1.checked_sub(1)?;
            session.groups().get(index).copied()
        });
        if let Some(group) = group {
            self.select_group(group);
        }
        let positioned = self.primary().and_then(|(container, index)| self.session.as_ref()?.entry(container)?.objects().get(index)?.position());
        if positioned.is_none() {
            let first = self.session.as_ref().and_then(|session| self.containers.iter().find_map(|container| {
                let index = session.entry(container.hash)?.objects().iter().position(|object| object.position().is_some())?;
                Some((container.hash, index))
            }));
            self.pick(first, PickMode::Replace);
        }
    }

    fn open_cell_target(&mut self, game_dir: &str, target: crate::cell_props::CellTarget) {
        if !self.cells_enabled {
            self.status = Some(("Outdoor placement editing is experimental: set SAB_WORKSHOP_TERRAIN_CELLS=1 and restart Workshop. Ground sculpting remains preview-only.".into(), true));
            return;
        }
        let candidate = sab_tools::cellsession::open_cell(Path::new(game_dir), target.cell).and_then(|(edit, session)| {
            let container = *edit.containers.get(target.list).ok_or("placement list no longer exists; restart Workshop to refresh cell props")?;
            let layer = edit.list_layer(target.list).ok_or("placement layer no longer exists")?;
            let selected = cell_target_object(&session, container, layer, &target)?;
            Ok((edit, session, selected))
        });
        match candidate {
            Ok((edit, session, selected)) => {
                if !self.confirm_discard("Editing this outdoor placement replaces the current session.") {
                    return;
                }
                self.set_session(session);
                self.pending_select = None;
                self.game_dir = game_dir.to_string();
                self.cell_input = target.cell.to_string();
                self.pick(Some(selected), PickMode::Replace);
                self.view.goto(target.pos, 15.0);
                self.right_inspector = true;
                self.status = Some((format!("Editing {} · cell {} · list {} · {:08x} from {}. Save project, then Stage mod; nothing is installed. {}", target.class_name, target.cell, target.list, target.tag, edit.source_rel, sab_tools::cellsession::COLLISION_CAVEAT), false));
                self.cell_edit = Some(edit);
            }
            Err(error) => self.status = Some((format!("Outdoor placement remains read-only: {error}. Current session unchanged."), true)),
        }
    }

    fn open(&mut self, game_dir: &str) {
        match Session::open(Path::new(game_dir)) {
            Ok(s) => {
                self.status = Some((format!("opened EditNodes from {game_dir}"), false));
                self.set_session(s);
            }
            Err(e) => {
                self.status = Some((
                    format!("could not open EditNodes: {e}. Check the game folder on the Settings page: it must be a retail Saboteur install (France\\loosefiles_BinPC.pack and DLC\\01 present)."),
                    true,
                ))
            }
        }
    }

    fn result(&mut self, r: Result<String, String>) {
        self.status = Some(match r {
            Ok(m) => (m, false),
            Err(e) => (e, true),
        });
        self.pos_for = None;
    }

    /// The 3D view's GPU frame for this UI frame, if it wants one (taken by the app, which renders it
    /// before egui paints).
    pub fn take_gpu_request(&mut self) -> Option<crate::place_gpu::DrawList> {
        self.view.gpu_request.take()
    }

    /// The egui texture the GPU scene renders into.
    pub fn set_gpu_texture(&mut self, id: Option<egui::TextureId>) {
        self.view.gpu_tex = id;
    }

    /// Give the 3D view the open full build to read meshes from, or say why there is none.
    pub fn set_mesh_source(&mut self, src: Option<&std::sync::Arc<dyn crate::source::GameSource>>, why_not: &str) {
        match src {
            Some(s) => {
                if self.view.meshes.as_ref().map_or(true, |m| m.source_label() != s.label()) {
                    self.view.meshes = Some(crate::place_mesh::MeshLib::new(s.clone()));
                }
            }
            None => {
                self.view.meshes = None;
                self.view.mesh_note = why_not.to_string();
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, game_dir: &str, export_dir: &str) {
        if self.game_dir != game_dir {
            self.game_dir = game_dir.to_string();
            self.terrain = crate::place_terrain::TerrainLink::default();
            self.terrain_patch = None;
            self.terrain_serial += 1;
            self.terrain_sent = u64::MAX;
            self.view.set_terrain(None);
        }
        if self.terrain_on && self.terrain.is_idle() && !game_dir.is_empty() {
            self.terrain.start_load(PathBuf::from(game_dir));
        }
        if self.export_dir != export_dir {
            self.export_dir = export_dir.to_string();
        }
        if self.session.is_none() && !self.tried_open && !game_dir.is_empty() {
            self.tried_open = true;
            self.open(game_dir);
        }
        self.apply_pending_select();
        if self.terrain.poll() {
            ctx.request_repaint();
        } else if !self.terrain.is_idle() && self.terrain.index().is_none() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        if self.live.poll() {
            self.status = Some(match self.live.status() {
                LiveStatus::Ready(r) => (format!("live readback: reply received for request {}{}", r.id, if r.fatals.is_empty() { "" } else { " (with FATAL errors; see the panel)" }), !r.fatals.is_empty()),
                LiveStatus::TimedOut => ("live readback: the game did not answer within 20 s. Is it running with sab_asi.asi and a save loaded?".into(), true),
                LiveStatus::Error(e) => (format!("live readback: {e}"), true),
                _ => ("live readback: waiting for the game".into(), false),
            });
        }
        if self.session.is_some() && !ctx.wants_keyboard_input() {
            self.shortcuts(ctx, export_dir);
        }

        // Page chrome spans the whole page, so the toolbar cannot squeeze the viewport sideways.
        egui::TopBottomPanel::top("pl_toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui, export_dir);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("pl_status").show(ctx, |ui| self.status_bar(ui));

        // Side panels start narrower on a small window so the view keeps most of the width.
        let w = ctx.screen_rect().width();
        let left_w = (w * 0.20).clamp(210.0, 270.0);
        let right_w = (w * 0.22).clamp(240.0, 330.0);
        if self.show_browser {
            egui::SidePanel::left("pl_nav").resizable(true).default_width(left_w).width_range(180.0..=(w * 0.25).clamp(200.0, 520.0)).show(ctx, |ui| self.nav(ui, game_dir));
        }
        if self.show_changes {
            egui::SidePanel::right("pl_side").resizable(true).default_width(right_w).width_range(210.0..=(w * 0.27).clamp(230.0, 420.0)).show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.right_inspector, true, "Inspector");
                    ui.selectable_value(&mut self.right_inspector, false, "Changes");
                });
                ui.separator();
                if self.right_inspector {
                    egui::ScrollArea::both().id_source("pl_inspector").auto_shrink([false, false]).show(ui, |ui| self.inspector(ui));
                } else {
                    egui::ScrollArea::vertical().id_source("pl_changes").auto_shrink([false, false]).show(ui, |ui| self.side(ui));
                }
            });
        }
        egui::CentralPanel::default().show(ctx, |ui| self.central(ui));
        self.shortcuts_window(ctx);
        self.class_picker_window(ctx);
        self.paste_special_window(ctx);
        self.autotest_tick(ctx);
    }

    /// Page-wide keys (Ariane's bindings where Saboteur supports the action; a toast where not).
    fn shortcuts(&mut self, ctx: &egui::Context, export_dir: &str) {
        let (m, k) = ctx.input(|i| {
            let p = |key| i.key_pressed(key);
            // "?" is Shift+/ on a US layout; the bridge sends typed text only to focused text fields.
            let question = i.modifiers.shift && i.key_pressed(egui::Key::Slash);
            (
                i.modifiers,
                [
                    p(egui::Key::Z),
                    p(egui::Key::Y),
                    p(egui::Key::S),
                    p(egui::Key::O),
                    p(egui::Key::B),
                    p(egui::Key::I),
                    p(egui::Key::F),
                    p(egui::Key::C),
                    p(egui::Key::G),
                    p(egui::Key::Delete) || p(egui::Key::Backspace),
                    p(egui::Key::Escape),
                    question,
                    p(egui::Key::X),
                    p(egui::Key::V),
                    p(egui::Key::D),
                ],
            )
        });
        let [z, y, s, o, b, i, f, c, _g, del, esc, question, x, v, d] = k;
        let cmd = m.command;
        if cmd && c {
            self.copy(ctx);
        }
        if cmd && x {
            self.cut(ctx);
        }
        if cmd && m.alt && v {
            self.paste_special = Some(PasteSpecial { at_target: true, keep_tags: self.clip_cut, offset: [0.0; 3] });
        } else if cmd && v {
            self.paste(ctx, m.shift);
        }
        if cmd && d {
            self.duplicate(ctx);
        }
        if !cmd && del {
            self.delete(ctx);
        }
        if cmd && !m.shift && z {
            if let Some(n) = self.undo() {
                self.view.toast(ctx, format!("Undo: {n}"));
            }
        } else if cmd && (y || (m.shift && z)) {
            if let Some(n) = self.redo() {
                self.view.toast(ctx, format!("Redo: {n}"));
            }
        }
        let (e_key, i_key) = ctx.input(|inp| (inp.key_pressed(egui::Key::E), inp.key_pressed(egui::Key::I)));
        if cmd && m.shift && e_key {
            if let Some(p) = rfd::FileDialog::new().set_title("Export prefab").add_filter("prefab", &["json"]).set_file_name("prefab.sabprefab.json").save_file() {
                let r = self.export_prefab_to(&p).map(|n| format!("exported {n} object(s) to {}", p.display()));
                self.result(r);
            }
        }
        if cmd && m.shift && i_key {
            if let Some(p) = rfd::FileDialog::new().set_title("Import prefab").add_filter("prefab", &["json"]).pick_file() {
                if let Err(e) = self.import_prefab_from(ctx, &p) {
                    self.result(Err(e));
                }
            }
        }
        if cmd && s {
            match self.project.clone() {
                Some(p) => self.save_project_to(&p),
                None => self.save_project(export_dir),
            }
            if let Some((msg, false)) = &self.status {
                let msg = msg.clone();
                self.view.toast(ctx, msg);
            }
        }
        if cmd && o {
            self.open_project();
        }
        if !cmd {
            if b {
                self.show_browser = !self.show_browser;
            }
            if i && !m.shift {
                self.show_info = !self.show_info;
            }
            if f {
                self.show_changes = !self.show_changes;
            }
            // G/R are the viewport's mouse transforms; they are handled there, with the view's own
            // cancel/commit rules. Shift+G (snap to ground) is still reported as unavailable.
            if m.shift && _g {
                self.view.toast(ctx, "Snap to ground isn't available: no ground data is decoded yet");
            }
            if esc && !self.view.modal_active() && self.primary().is_some() {
                self.set_selection(Vec::new());
            }
            if question {
                self.show_shortcuts = !self.show_shortcuts;
            }
        }
    }

    /// The ClassName picker: the classes this install actually places, ranked by use, plus a typed
    /// template name for classes the data does not contain. Changing ClassName is one undo step and is
    /// labelled statically checked until an in-game run covers it.
    fn class_picker_window(&mut self, ctx: &egui::Context) {
        let Some(current) = self.class_picker.clone() else { return };
        let mut filter = current;
        let mut open = true;
        let mut pick: Option<String> = None;
        let current_class = self.primary().and_then(|(c, i)| self.session.as_ref()?.entry(c)?.objects().get(i)?.class_name()).unwrap_or_default();
        let schema = self.schema.as_ref();
        egui::Window::new("Change ClassName").open(&mut open).default_size([460.0, 540.0]).resizable(true).show(ctx, |ui| {
            ui.label(theme::data_text(
                "ClassName is the object's template/archetype. Changing it can change which mesh resolves and what the game spawns; it is recorded as one undo step.",
                10.0,
                theme::DIM,
            ));
            ui.label(theme::data_text(format!("current: {current_class}"), 10.5, theme::COLD));
            ui.horizontal(|ui| {
                ui.label(theme::data_text("search", 10.0, theme::FAINT));
                ui.add(egui::TextEdit::singleline(&mut filter).hint_text("class name (or type a new one)").desired_width(f32::INFINITY));
            });
            ui.label(theme::data_text(
                format!("{} class(es), {} classed object(s) observed in this install's EditNodes data", schema.map_or(0, |s| s.classes.len()), schema.map_or(0, |s| s.objects)),
                10.0,
                theme::FAINT,
            ));
            egui::ScrollArea::vertical().id_source("pl_class_picker").max_height(360.0).show(ui, |ui| {
                let q = filter.to_ascii_lowercase();
                if let Some(schema) = schema {
                    for (name, count) in schema.ranked() {
                        if !q.is_empty() && !name.to_ascii_lowercase().contains(&q) {
                            continue;
                        }
                        if ui.selectable_label(name == current_class, format!("{name}  ·  {count} object(s)")).clicked() {
                            pick = Some(name.to_string());
                        }
                    }
                }
            });
            ui.separator();
            ui.label(theme::data_text("Not in the data? A GameTemplates ClassName can be typed above and applied.", 9.5, theme::FAINT));
            let typed = filter.trim().to_string();
            if ui.add_enabled(!typed.is_empty(), egui::Button::new("Apply typed ClassName")).on_hover_text("One undo step; the staged pack records the old bytes").clicked() {
                pick = Some(typed);
            }
        });
        if let Some(name) = pick {
            if let Some((c, i)) = self.primary() {
                let r = match self.session.as_mut() {
                    Some(s) => s.set_class_name(c, i, &name).map(|_| format!("ClassName set to {name} (one undo step; statically checked)")),
                    None => Err("no session".to_string()),
                };
                self.result(r);
                self.pos_for = None;
                self.view.request_focus();
            }
            self.class_picker = None;
        } else if open {
            self.class_picker = Some(filter);
        } else {
            self.class_picker = None;
        }
    }

    /// Paste Special's dialog: where the clipboard's centre goes, an extra offset, and whether a
    /// cut's original tags may be kept. Applying is one undo step and never rewrites references.
    fn paste_special_window(&mut self, ctx: &egui::Context) {
        let Some(mut ps) = self.paste_special.take() else { return };
        let mut open = true;
        let mut close = false;
        let mut apply = false;
        let count = self.clipboard.len();
        egui::Window::new("Paste special").open(&mut open).default_size([420.0, 300.0]).collapsible(false).show(ctx, |ui| {
            ui.label(theme::data_text(format!("{count} object(s) on the clipboard"), 10.5, theme::TX));
            ui.horizontal(|ui| {
                ui.selectable_value(&mut ps.at_target, true, "At camera target");
                ui.selectable_value(&mut ps.at_target, false, "In place");
            });
            ui.horizontal(|ui| {
                ui.label(theme::data_text("extra offset", 10.0, theme::FAINT));
                for (k, axis) in ["dx", "dy", "dz"].iter().enumerate() {
                    ui.label(theme::data_text(*axis, 10.0, theme::FAINT));
                    ui.add(egui::DragValue::new(&mut ps.offset[k]).speed(0.05).max_decimals(4).suffix(" m"));
                }
            });
            ui.checkbox(&mut ps.keep_tags, "keep original tags when they are free (a cut's paste)").on_hover_text(
                "Copies always get new name_N tags. A cut's originals are free again, so its paste can keep them.",
            );
            ui.label(theme::data_text(
                "References inside copied LuaParams are never rewritten. One Apply = one undo step.",
                9.5,
                theme::FAINT,
            ));
            ui.separator();
            ui.horizontal(|ui| {
                apply = ui.add_enabled(count > 0, egui::Button::new("Paste")).clicked();
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close {
            open = false;
        }
        if apply && count > 0 {
            self.paste_with(ctx, &ps);
        } else if open {
            self.paste_special = Some(ps);
        }
    }

    /// Paste the clipboard with explicit placement choices (`PasteSpecial`).
    fn paste_with(&mut self, ctx: &egui::Context, ps: &PasteSpecial) {
        if self.clipboard.is_empty() {
            return;
        }
        let mut offset = ps.offset;
        if ps.at_target {
            let ps_pos: Vec<[f32; 3]> = self.clipboard.iter().filter_map(|(_, n)| n.position()).collect();
            let n = ps_pos.len().max(1) as f32;
            let c = ps_pos.iter().fold([0.0; 3], |a, p| [a[0] + p[0] / n, a[1] + p[1] / n, a[2] + p[2] / n]);
            let t = self.view.target();
            offset = [offset[0] + t[0] - c[0], offset[1] + t[1] - c[1], offset[2] + t[2] - c[2]];
        }
        let label = if ps.at_target { "Paste special" } else { "Paste special (in place)" };
        self.insert_copies(ctx, self.clipboard.clone(), offset, ps.keep_tags, label);
        self.paste_special = None;
    }

    /// B1: ask the running game for the selection's live positions. One request at a time; the game's
    /// reply is parsed in `LiveLink::poll` from the devlog. `sab_cmd.lua` is backed up by the live
    /// link and restored when asked, on game switch, and on app close.
    fn live_read(&mut self) {
        let Some(s) = self.session.as_ref() else { return };
        if self.game_dir.is_empty() {
            self.status = Some(("live readback: no game folder is set".into(), true));
            return;
        }
        let selected = self.selected();
        if selected.is_empty() {
            self.status = Some(("live readback: select at least one object first".into(), true));
            return;
        }
        let mut objects = Vec::new();
        let mut missing = 0usize;
        for (c, i) in selected {
            let Some(o) = s.entry(c).and_then(|e| e.objects().get(i)) else { continue };
            match (s.object_name(o.tag()), o.position()) {
                (Some(path), Some(pos)) => objects.push(QueryObject { path, editor_pos: pos }),
                _ => missing += 1,
            }
        }
        let node = self.primary().and_then(|(c, _)| lua_node_name(c));
        let rev = s.revision();
        match self.live.ask(Path::new(&self.game_dir), rev, node, objects) {
            Ok(id) => {
                self.live_missing = missing;
                let extra = if missing > 0 { format!("; {missing} selected object(s) have no recovered name and were not queried") } else { String::new() };
                self.status = Some((format!("live readback: request {id} written to sab_cmd.lua; waiting for the game{extra}"), false));
            }
            Err(e) => self.status = Some((format!("live readback: {e}"), true)),
        }
    }

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        if !self.show_shortcuts {
            return;
        }
        let mut open = true;
        egui::Window::new("Keyboard and mouse").open(&mut open).default_size([720.0, 560.0]).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("filter");
                ui.text_edit_singleline(&mut self.shortcut_filter);
                if ui.button("Clear").clicked() {
                    self.shortcut_filter.clear();
                }
            });
            ui.label(theme::data_text("origin: \"Ariane\" = follows Ariane's control scheme; \"places\" = this editor's own", 10.0, theme::FAINT));
            ui.separator();
            let q = self.shortcut_filter.to_ascii_lowercase();
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("pl_shortcuts").striped(true).num_columns(4).show(ui, |ui| {
                    for (group, keys, what, origin) in SHORTCUTS {
                        if !q.is_empty() && ![group, keys, what, origin].iter().any(|t| t.to_ascii_lowercase().contains(&q)) {
                            continue;
                        }
                        ui.label(theme::data_text(*group, 10.0, theme::FAINT));
                        ui.label(theme::data_text(*keys, 10.5, theme::TX));
                        ui.label(theme::data_text(*what, 10.5, theme::DIM));
                        ui.label(theme::data_text(*origin, 10.0, if *origin == "Ariane" { theme::COLD } else { theme::EMBER }));
                        ui.end_row();
                    }
                });
            });
        });
        self.show_shortcuts = open;
    }

    /// Build or refresh the area list. Edits move object indices (insert/remove), so the list is
    /// rebuilt whenever the session's revision changes. The timing is kept because the build reads the
    /// pack index and walks every EditNodes object; the UI shows it so a slow install is visible
    /// instead of looking like a freeze.
    fn ensure_areas(&mut self, game_dir: &str) {
        let Some(s) = self.session.as_ref() else { return };
        if self.areas.is_some() && self.areas_rev == s.revision() {
            return;
        }
        let started = std::time::Instant::now();
        self.areas_rev = s.revision();
        self.areas = Some(areas::build(Path::new(game_dir), s).map(std::sync::Arc::new));
        self.areas_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.areas_builds += 1;
    }

    /// The observed per-class parameter schema (see `placement_schema`). Rebuilt with the data; the
    /// scan is one pass over the effective EditNodes objects.
    fn ensure_schema(&mut self) {
        let Some(s) = self.session.as_ref() else { return };
        if self.schema.is_some() && self.schema_rev == s.revision() {
            return;
        }
        self.schema_rev = s.revision();
        self.schema = Some(crate::placement_schema::Schema::build(s));
    }

    /// The area browser: pick a destination (interior, exterior bookmark or technical map object),
    /// list what it holds, and jump there. Opening an area only moves the camera and the selection;
    /// it never touches the edit journal. The whole browser body is one scroll area, so the object
    /// list and the resource details can never push each other under the status bar.
    fn area_browser(&mut self, ui: &mut egui::Ui, game_dir: &str) {
        self.ensure_areas(game_dir);
        let mut open_idx = None;
        let mut open_cell_req: Option<u32> = None;
        let mut pick_obj: Option<(ObjRef, bool)> = None;
        ui.push_id("pl_areas", |ui| {
            let Some(list) = self.areas.as_ref().map(|r| match r {
                Ok(a) => Ok(a.clone()),
                Err(e) => Err(e.clone()),
            }) else {
                return;
            };
            let list = match list {
                Ok(l) => l,
                Err(e) => {
                    ui.add(egui::Label::new(theme::data_text(
                        format!("Area list unavailable: {e}. The browser reads France\\loosefiles_BinPC.pack from the game folder; check it on the Settings page. You can still pick objects from the Containers tab."),
                        10.0,
                        theme::RED,
                    )).wrap());
                    return;
                }
            };
            ui.horizontal(|ui| {
                for f in AreaFilter::ALL {
                    ui.selectable_value(&mut self.area_kind, f, f.label());
                }
            });
            if self.cells_enabled {
                ui.collapsing("Terrain cell (experimental)", |ui| {
                    ui.add(egui::Label::new(theme::data_text(
                        "Edit existing placements: move, rotate, duplicate or delete; save/reopen projects and stage a reversible France patch. Double-click an outdoor origin marker to open its exact cell/list/object. Ground sculpting is preview-only. Collision is not rebuilt; cross-cell migration and patch/DLC-owned cells are unsupported.",
                        9.5,
                        theme::FAINT,
                    )).wrap());
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.cell_input).hint_text("cell id, e.g. 1700865").desired_width(150.0));
                        if ui.button("Open cell").clicked() {
                            match self.cell_input.trim().parse::<u32>() {
                                Ok(id) => open_cell_req = Some(id),
                                Err(_) => self.status = Some(("enter a numeric cell id".into(), true)),
                            }
                        }
                    });
                    if let Some(c) = &self.cell_edit {
                        ui.label(theme::data_text(format!("cell {} is open (won by {}). Stage mod writes a patch megapack; EditNodes staging is off in this mode.", c.cell, c.source_rel), 9.5, theme::EMBER));
                    }
                });
            }
            // Go to Object: search by name, class, tag (8 hex digits) or container#index; picking a result selects it and
            // frames it. Navigation only: it never touches the edit journal.
            ui.add(egui::TextEdit::singleline(&mut self.goto_query).hint_text("go to object: name, class, tag or container#index").desired_width(f32::INFINITY));
            let gq = self.goto_query.trim().to_string();
            if gq.is_empty() {
                self.goto_cache = None;
            } else if self.goto_cache.as_ref().map_or(true, |(q, _, _)| *q != gq) {
                if let Some(s) = self.session.as_ref() {
                    let (h, total) = sab_tools::areas::find_objects(s, &gq, 50);
                    self.goto_cache = Some((gq.clone(), h, total));
                }
            }
            let mut goto_pick: Option<sab_tools::areas::ObjectHit> = None;
            match (&self.goto_cache, self.session.is_some()) {
                (_, false) if !gq.is_empty() => {
                    ui.label(theme::data_text("open EditNodes first: Go to Object searches the loaded containers", 9.5, theme::EMBER));
                }
                (Some((_, hits, total)), _) => {
                    if hits.is_empty() {
                        ui.label(theme::data_text("no match (only objects in the loaded EditNodes containers are searched; unloaded regions are not)", 9.5, theme::EMBER));
                    } else {
                        ui.label(theme::data_text(format!("{} of {} match(es) · click: select and frame", hits.len(), total), 9.5, theme::FAINT));
                        for h in hits.iter().take(12) {
                            let at = h.pos.map(|p| format!("({:.0}, {:.0}, {:.0})", p[0], p[1], p[2])).unwrap_or_else(|| "no position".into());
                            let r = ui.selectable_label(false, theme::data_text(format!("{} · {} · {:08x} · {at}", h.name, h.class_name, h.tag), 10.0, theme::TX));
                            if r.clicked() {
                                goto_pick = Some(h.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
            if let Some(h) = goto_pick {
                self.pick(Some(h.obj), PickMode::Replace);
                match h.pos {
                    Some(p) => {
                        self.view.goto(p, 6.0);
                        self.status = Some((format!("selected {} ({:08x}) and framed it", h.name, h.tag), false));
                    }
                    None => self.status = Some((format!("selected {} ({:08x}); it has no usable position, so there is nothing to frame", h.name, h.tag), true)),
                }
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.area_filter).hint_text("search areas").desired_width(f32::INFINITY));
            let hits = sab_tools::areas::search(&list, &self.area_filter);
            let rows: Vec<(usize, &Area)> = hits.into_iter().filter_map(|i| list.get(i).map(|a| (i, a))).filter(|(_, a)| self.area_kind.admits(a.kind)).collect();
            ui.label(theme::data_text(format!("{} of {} areas · double-click to open", rows.len(), list.len()), 10.0, theme::FAINT));
            egui::ScrollArea::vertical().id_source("pl_area_scroll").auto_shrink([false, false]).show(ui, |ui| {
                for &(i, a) in &rows {
                    let tag = match a.kind {
                        Kind::Interior => "I",
                        Kind::Exterior => "E",
                        Kind::Technical => "T",
                    };
                    let row = ui.selectable_label(self.area_sel == Some(i), theme::data_text(format!("{}  [{tag}]  {}", a.name, a.objects.len()), 10.0, theme::TX));
                    if row.clicked() {
                        self.area_sel = Some(i);
                    }
                    if row.double_clicked() {
                        open_idx = Some(i);
                    }
                    row.on_hover_text(a.availability());
                }
                if let Some(a) = self.area_sel.and_then(|i| list.get(i)) {
                    // Objects first: the point of opening an area is to select something in it.
                    let count = format!("{} object(s) · click selects, double-click focuses", a.objects.len());
                    ui.add(egui::Label::new(theme::data_text(count.clone(), 9.5, theme::FAINT)).truncate()).on_hover_text(count);
                    let selected = self.selected();
                    for o in &a.objects {
                        let r = ui.selectable_label(selected.contains(&o.obj), theme::data_text(format!("{}  ·  {}", o.name, o.class_name), 10.0, theme::TX));
                        if r.clicked() || r.double_clicked() {
                            pick_obj = Some((o.obj, r.double_clicked()));
                        }
                    }
                    // One line each, full text on hover: details stay compact and sit below the
                    // objects so they never displace the list.
                    let mut line = |ui: &mut egui::Ui, text: String, colour: egui::Color32| {
                        ui.add(egui::Label::new(theme::data_text(text.clone(), 9.5, colour)).truncate()).on_hover_text(text);
                    };
                    line(ui, format!("anchor: {}", a.anchor_note), theme::DIM);
                    match sab_tools::areas::verified_landing(&a.name) {
                        Some(l) => {
                            let f = |p: Option<[f32; 3]>| p.map(|p| format!("({:.1}, {:.1}, {:.1})", p[0], p[1], p[2])).unwrap_or_else(|| "not verified".into());
                            line(ui, format!("in game 2026-11-03: inside {} · outside {} · {}", f(l.inside), f(l.outside), l.note), theme::DIM);
                        }
                        None => line(ui, "in game: no verified landing for this area".to_string(), theme::DIM),
                    }
                    if a.kind != Kind::Technical || a.primary.is_some() {
                        line(ui, format!("geometry: {}", a.primary_note), theme::DIM);
                    }
                    if !a.script.is_empty() {
                        line(ui, format!("script: {}", a.script), theme::DIM);
                    }
                    // Resources are listed apart from the camera anchor: the map object drawn for the
                    // room, the other map objects whose box contains the anchor, and the EditNodes
                    // containers whose objects the area's list can select (staging exports these).
                    let map_objects = match a.map_objects.len() {
                        0 => "none".to_string(),
                        1..=4 => a.map_objects.iter().map(|m| format!("{} (0x{:08x})", m.name, m.hash)).collect::<Vec<_>>().join(", "),
                        n => format!("{} map objects (first: {})", n, a.map_objects[0].name),
                    };
                    let containers = a
                        .containers()
                        .into_iter()
                        .map(|(c, n)| format!("{} ({n})", short(editnodes::tag_name(c), c)))
                        .collect::<Vec<_>>();
                    let containers = if containers.is_empty() { "none".to_string() } else { containers.join(", ") };
                    egui::CollapsingHeader::new(theme::data_text("resources and rebuild cost", 9.5, theme::FAINT))
                        .id_source("pl_area_resources")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(theme::data_text(format!("map object: {map_objects}"), 9.0, theme::DIM)).wrap());
                            ui.add(egui::Label::new(theme::data_text(format!("EditNodes containers: {containers}"), 9.0, theme::DIM)).wrap());
                            ui.add(egui::Label::new(theme::data_text(
                                format!("area list rebuilt in {:.1} ms ({} build(s) this session); rebuilt after edits because insert/remove moves indices", self.areas_ms, self.areas_builds.max(1)),
                                9.0,
                                theme::FAINT,
                            )).wrap());
                        });
                }
            });
        });
        if let Some(i) = open_idx {
            self.open_area(i);
        }
        if let Some(id) = open_cell_req {
            self.open_cell(game_dir, id);
        }
        if let Some((o, focus)) = pick_obj {
            self.pick(Some(o), PickMode::Replace);
            if focus {
                self.view.request_focus();
            }
        }
    }

    /// Open an area: select its anchor (or nearest object) so the 3D view has something to show, then
    /// aim the camera at the anchor. The room loads through the view's usual path.
    fn open_area(&mut self, i: usize) {
        let Some(Ok(list)) = self.areas.as_ref() else { return };
        let Some(a) = list.get(i).cloned() else { return };
        self.area_sel = Some(i);
        self.current_area = Some(i);
        let Some(anchor) = a.anchor else {
            self.status = Some((format!("{} cannot be opened: {}. Use the Containers tab to reach its objects.", a.name, a.anchor_note), true));
            return;
        };
        match a.anchor_obj.or_else(|| a.objects.first().map(|o| o.obj)) {
            Some(o) => self.pick(Some(o), PickMode::Replace),
            None => self.status = Some((format!("{}: no EditNodes object here to select; showing the map object only", a.name), false)),
        }
        self.view.goto(anchor, if a.kind == Kind::Interior { 22.0 } else { 40.0 });
        self.status = Some((format!("opened {} ({}): {}", a.name, a.kind.label(), a.availability()), false));
    }

    fn nav(&mut self, ui: &mut egui::Ui, game_dir: &str) {
        ui.add_space(6.0);
        if self.session.is_none() {
            ui.add_space(8.0);
            ui.label(theme::data_text(format!("game folder: {game_dir}"), 10.0, theme::DIM));
            if theme::action_button(ui, "Open EditNodes", !game_dir.is_empty()).clicked() {
                self.open(game_dir);
            }
            return;
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.nav_areas, true, "Areas");
            ui.selectable_value(&mut self.nav_areas, false, "Containers");
        });
        ui.separator();
        if self.nav_areas {
            self.area_browser(ui, game_dir);
            return;
        }
        ui.horizontal(|ui| {
            ui.label(theme::data_text("filter", 10.0, theme::FAINT));
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("name or 0xhash").desired_width(f32::INFINITY));
        });
        let q = self.filter.to_ascii_lowercase();
        let rows: Vec<&ContainerInfo> = self
            .containers
            .iter()
            .filter(|c| q.is_empty() || short(c.name, c.hash).to_ascii_lowercase().contains(&q) || format!("0x{:08x}", c.hash).contains(&q))
            .collect();
        ui.label(theme::data_text(format!("{} of {} containers", rows.len(), self.containers.len()), 10.0, theme::FAINT));
        let mut pick = None;
        egui::ScrollArea::vertical().id_source("pl_containers").max_height((ui.available_height() * 0.4).max(150.0)).auto_shrink([false, true]).show_rows(ui, 18.0, rows.len(), |ui, range| {
            for c in &rows[range] {
                let label = format!("{}{}  ({})", short(c.name, c.hash), if c.in_dlc { "  [DLC/01]" } else { "" }, c.objects);
                if ui.selectable_label(self.sel_container == Some(c.hash), theme::data_text(label, 10.5, theme::TX)).clicked() {
                    pick = Some(c.hash);
                }
            }
        });
        if let Some(h) = pick {
            self.sel_container = Some(h);
            self.sel_object = None;
            self.extra.clear();
            self.pos_for = None;
        }
        ui.separator();
        let (Some(s), Some(c)) = (self.session.as_ref(), self.sel_container) else {
            ui.label(theme::data_text("select a container", 10.0, theme::FAINT));
            return;
        };
        let Some(entry) = s.entry(c) else { return };
        ui.label(theme::disp_text(format!("OBJECTS ({})", entry.objects().len()), 11.0, theme::TX));
        let mut pick_o = None;
        let selected = self.selected();
        egui::ScrollArea::vertical().id_source("pl_objects").auto_shrink([false, false]).show(ui, |ui| {
            for (i, o) in entry.objects().iter().enumerate() {
                let name = s.object_name(o.tag()).map(|n| n.rsplit('\\').next().unwrap_or(&n).to_string()).unwrap_or_else(|| format!("0x{:08x}", o.tag()));
                let label = format!("{i:>3}  {name}  ·  {}  {}", o.type_name().unwrap_or_default(), o.class_name().unwrap_or_default());
                if ui.selectable_label(selected.contains(&(c, i)), theme::data_text(label, 10.5, theme::TX)).clicked() {
                    pick_o = Some(i);
                }
            }
        });
        if let Some(i) = pick_o {
            let m = ui.input(|inp| inp.modifiers);
            let mode = if m.shift { PickMode::Add } else if m.ctrl { PickMode::Toggle } else { PickMode::Replace };
            self.pick(Some((c, i)), mode);
            if mode == PickMode::Replace {
                self.view.request_focus();
            }
        }
    }

    /// The always-visible command row: undo/redo, project, stage, and the last status message.
    fn toolbar(&mut self, ui: &mut egui::Ui, export_dir: &str) {
        let Some(s) = self.session.as_ref() else { return };
        let (can_undo, can_redo, active) = (s.can_undo(), s.can_redo(), s.active().len());
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(can_undo, egui::Button::new("Undo")).clicked() {
                self.undo();
            }
            if ui.add_enabled(can_redo, egui::Button::new("Redo")).clicked() {
                self.redo();
            }
            ui.separator();
            let save_resp = ui.button("Save project…").on_hover_text("Save your edits as a project file you can reopen. This is not the mod");
            #[cfg(test)]
            self.test_rects.insert("save", save_resp.rect);
            if save_resp.clicked() {
                self.save_project(export_dir);
            }
            let open_resp = ui.button("Open project…");
            #[cfg(test)]
            self.test_rects.insert("open", open_resp.rect);
            if open_resp.clicked() {
                self.open_project();
            }
            if ui
                .add_enabled(!self.clipboard.is_empty(), egui::Button::new("Paste special…"))
                .on_hover_text("Paste with an explicit placement, offset and tags (Ctrl+Alt+V)")
                .clicked()
            {
                self.paste_special = Some(PasteSpecial { at_target: true, keep_tags: self.clip_cut, offset: [0.0; 3] });
            }
            ui.separator();
            let stage_resp = ui
                .add_enabled(active > 0, egui::Button::new("Stage mod"))
                .on_hover_text("Write the mod files (DLC/01 EditNodes override plus install and restore scripts) to a new folder. Nothing is installed.");
            #[cfg(test)]
            self.test_rects.insert("stage", stage_resp.rect);
            if stage_resp.clicked() {
                self.stage(export_dir);
            }
            ui.separator();
            // Panel visibility is also on B / I / F; buttons keep it discoverable.
            if ui.selectable_label(self.show_browser, "Browser").on_hover_text("B").clicked() {
                self.show_browser = !self.show_browser;
            }
            if ui.selectable_label(self.show_changes, "Panels").on_hover_text("Show or hide the inspector/changes side panel (I / F)").clicked() {
                self.show_changes = !self.show_changes;
            }
            if let Some(name) = self.current_area.and_then(|i| self.areas.as_ref().and_then(|a| a.as_ref().ok()).and_then(|l| l.get(i))).map(|a| format!("{} ({})", a.name, a.kind.label())) {
                ui.separator();
                ui.label(theme::data_text(format!("area: {name}"), 10.0, theme::COLD));
            }
        });
    }

    /// One footer line: the last action's message on the left, the session's state on the right. Long
    /// paths and errors are truncated with the full text on hover, so they cannot squeeze the viewport.
    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.columns(2, |cols| {
            match &self.status {
                Some((m, err)) => {
                    cols[0].add(egui::Label::new(theme::data_text(m, 10.0, if *err { theme::RED } else { theme::COLD })).truncate()).on_hover_text(m);
                }
                None => {
                    cols[0].label(theme::data_text("ready", 10.0, theme::FAINT));
                }
            }
            cols[1].with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let selected = self.selected().len();
                if selected > 1 {
                    ui.label(theme::data_text(format!("{selected} selected"), 10.0, theme::TX));
                }
                if let Some(s) = self.session.as_ref() {
                    let (active, saved) = (s.active().len(), self.is_dirty());
                    let what = match (&self.project, saved) {
                        (None, false) => "no unsaved edits".to_string(),
                        (None, true) => "UNSAVED edits (no project file)".to_string(),
                        (Some(p), false) => format!("project: {}", p.display()),
                        (Some(p), true) => format!("UNSAVED changes since {}", p.display()),
                    };
                    ui.label(theme::data_text(format!("{active} active edit(s) · {what}"), 10.0, if saved { theme::EMBER } else { theme::FAINT }));
                }
            });
        });
    }

    fn stage(&mut self, export_dir: &str) {
        if let (Some(edit), Some(s)) = (self.cell_edit.as_ref(), self.session.as_ref()) {
            // terrain-cell mode: NEVER the EditNodes staging (the containers are synthetic); a France patch megapack instead
            let out = Path::new(export_dir).join(format!("cell_stage_{}_{}", edit.cell, stamp()));
            match sab_tools::cellsession::stage_cell(Path::new(&self.game_dir), edit, s, &out) {
                Ok(o) => {
                    self.staged_dir = Some(o.out_dir.clone());
                    self.status = Some((format!("staged {} to {} ({}); static checks {}", o.slot_name, o.out_dir.display(), &o.artifact_sha[..12], if o.all_pass { "PASS" } else { "FAILED: do not install" }), !o.all_pass));
                    self.report = Some(o.report);
                }
                Err(e) => self.status = Some((format!("terrain-cell export refused: {e}. Nothing was written to the game folder."), true)),
            }
            return;
        }
        let Some(s) = self.session.as_ref() else { return };
        let out = Path::new(export_dir).join(format!("editnodes_stage_{}", stamp()));
        match s.stage(&out) {
            Ok(o) => {
                self.staged_dir = Some(o.out_dir.clone());
                self.status = Some((
                    format!("staged to {} ({}); static checks {}", o.out_dir.display(), &o.artifact_sha[..12], if o.all_pass { "PASS" } else { "FAILED: do not install" }),
                    !o.all_pass,
                ));
                self.report = Some(o.report);
            }
            Err(e) => self.status = Some((format!("stage failed: {e}. Nothing was installed. Use \"Choose export folder…\" in the Changes tab to pick a writable folder (not inside the game folder or Downloads)."), true)),
        }
    }

    /// The main surface: the 3D view, which takes all the remaining height.
    fn central(&mut self, ui: &mut egui::Ui) {
        self.update_terrain_patch();
        let selected = self.selected();
        let Some(s) = self.session.as_ref() else {
            ui.add_space(28.0);
            ui.label(theme::poster_text("PLACEMENTS", 22.0, theme::TX));
            ui.label(theme::data_text("Open an area or pick an object on the left; the view shows it here.", 11.0, theme::DIM));
            ui.add_space(10.0);
            ui.label(theme::data_text(
                "EditNodes exports a DLC/01 override; experimental outdoor placement editing exports a France patch. \
                 Open an area, then double-click a cell origin marker to request editing. Ground sculpting is preview-only; visual changes do not rebuild collision.",
                10.0,
                theme::FAINT,
            ));
            return;
        };
        if !self.show_view {
            return;
        }
        self.view.ensure_props(&self.game_dir);
        self.view.set_edit_cell(self.cell_edit.as_ref().map(|edit| edit.cell));
        if let Some(edit) = &self.cell_edit {
            ui.add(egui::Label::new(theme::data_text(format!("CELL {} · placement edits export; ground sculpt does not · collision/navigation are not rebuilt", edit.cell), 10.0, theme::EMBER)).wrap());
        }
        let h = (ui.available_height() - 64.0).max(160.0);
        let primary = self.sel_container.zip(self.sel_object);
        let view_action = self.view.show(ui, s, primary, &selected, h);
        match view_action {
            Some(ViewAction::OpenCell(target)) => {
                let game = self.game_dir.clone();
                self.open_cell_target(&game, target);
            }
            Some(ViewAction::Pick(hit, mode)) => self.pick(hit, mode),
            Some(ViewAction::PickMany(hits, mode)) => {
                let n = hits.len();
                self.pick_many(hits, mode);
                self.view.toast(ui.ctx(), format!("Box: {n} object(s)"));
            }
            Some(ViewAction::Move(moves)) => self.move_many(&moves),
            Some(ViewAction::Rotate(objs, r, pivot)) => self.rotate_many(ui.ctx(), &objs, r, pivot),
            None => {}
        }
        // terrain under the cursor (read-only): only once the terrain index is loaded
        let hover = match (self.terrain.index(), ui.input(|i| i.pointer.hover_pos())) {
            (Some(idx), Some(pos)) => self.view.hover_ray(pos).map(|(o, d)| match crate::place_terrain::pick(idx, o.to_array(), d.to_array(), 5000.0) {
                Some(h) => {
                    self.terrain_hit = Some(h.position);
                    format!("terrain under the cursor: ({:.2}, {:.2}, {:.2}), {:.1} m away", h.position[0], h.position[1], h.position[2], h.distance)
                }
                None => "terrain under the cursor: none (a terrain hole, the sky, or beyond 5 km)".to_string(),
            }),
            _ => None,
        };
        if hover.is_some() {
            self.terrain_hover = hover;
        }
        if self.terrain.index().is_some() {
            if let Some(t) = &self.terrain_hover {
                ui.label(theme::data_text(t.clone(), 9.5, theme::DIM));
            }
        }
    }

    /// The selected object's name, source, position fields and parameters (the right panel).
    /// Keep the terrain preview patch current: sampled around the camera target from the index (re-sampled when the view moves away, unless it has sculpt edits,
    /// which are never discarded silently), and handed to the 3D view as geometry with a serial that changes with it.
    fn update_terrain_patch(&mut self) {
        use crate::place_terrain::Patch;
        if self.terrain_demo_pending && !self.game_dir.is_empty() {
            if self.terrain.is_idle() {
                self.terrain.start_load(PathBuf::from(&self.game_dir));
            }
            if self.terrain.index().is_some() {
                self.terrain_on = true;
            }
        }
        if !self.terrain_on {
            if self.terrain_sent != u64::MAX {
                self.view.set_terrain(None);
                self.terrain_sent = u64::MAX;
            }
            return;
        }
        let Some(idx) = self.terrain.index() else { return };
        let t = self.view.target_xyz();
        let ref_y = (|| self.session.as_ref()?.entry(self.sel_container?)?.objects().get(self.sel_object?)?.position())().map_or(t[1], |p| p[1]);
        let far = |p: &Patch| (t[0] - p.cx).abs() > 64.0 || (t[2] - p.cz).abs() > 64.0;
        let need = match &self.terrain_patch {
            None => true,
            Some(p) => !p.has_edits() && far(p),
        };
        if need {
            match Patch::sample(idx, t[0], t[2], 160.0, 4.0, ref_y) {
                Ok(p) => {
                    self.terrain_patch = Some(p);
                    self.terrain_serial += 1;
                }
                Err(e) => {
                    self.status = Some((format!("terrain preview: {e}"), true));
                    self.terrain_on = false;
                    return;
                }
            }
        }
        if self.terrain_demo_pending && self.terrain_patch.is_some() {
            self.terrain_demo_pending = false;
            self.terrain_demo();
        }
        if let Some(p) = &self.terrain_patch {
            let serial = self.terrain_serial * 1_000_000 + p.rev;
            if self.terrain_sent != serial {
                self.view.set_terrain(Some((serial, std::sync::Arc::new(p.geom()))));
                self.terrain_sent = serial;
            }
        }
    }

    /// One sculpt dab at the last terrain point the cursor touched in the 3D view (else the camera target). PREVIEW ONLY.
    fn terrain_dab(&mut self, brush: crate::place_terrain::Brush) {
        let at = self.terrain_hit.map(|h| [h[0] as f32, h[2] as f32]).unwrap_or_else(|| {
            let t = self.view.target_xyz();
            [t[0], t[2]]
        });
        let (r, s) = (self.terrain_brush_radius, self.terrain_brush_strength);
        if let Some(p) = self.terrain_patch.as_mut() {
            let n = p.dab(brush, at[0], at[1], r, s);
            self.status = Some((if n > 0 { format!("terrain preview: {brush:?} at ({:.0}, {:.0}), {n} nodes (preview only: not saved, not exported, not in the game)", at[0], at[1]) } else { "terrain preview: nothing changed (outside the patch, or over a terrain hole)".into() }, n == 0));
        }
    }

    /// Terrain read-out (read-only) and the PREVIEW sculpt tools: load the index on demand, then the terrain under the selected object.
    fn terrain_panel(&mut self, ui: &mut egui::Ui) {
        ui.add(egui::Label::new(theme::data_text(self.terrain.status_line(), 9.5, theme::DIM)).wrap());
        if self.terrain.is_idle() && !self.game_dir.is_empty() {
            if ui
                .button("Load terrain index")
                .on_hover_text("Reads the base terrain once in the background (a few seconds, about 160 MB) for the height under the selected object and terrain picking under the cursor. Read-only: nothing is edited.")
                .clicked()
            {
                self.terrain.start_load(PathBuf::from(&self.game_dir));
            }
        }
        let pos = (|| self.session.as_ref()?.entry(self.sel_container?)?.objects().get(self.sel_object?)?.position())();
        if let (Some(idx), Some(p)) = (self.terrain.index(), pos) {
            let line = match crate::place_terrain::ground_under(idx, p) {
                Ok(r) => crate::place_terrain::describe_ground(&r, p),
                Err(e) => format!("terrain: query failed: {e}"),
            };
            ui.add(egui::Label::new(theme::data_text(line, 9.5, theme::TX)).wrap());
        }
        if self.terrain.index().is_some() {
            use crate::place_terrain::Brush;
            ui.checkbox(&mut self.terrain_on, "show terrain").on_hover_text("Draws the real terrain around the camera target in the 3D view (read-only). Holes (roads, buildings, interiors) stay empty.");
            if self.terrain_on {
                ui.add(egui::Label::new(theme::data_text("TERRAIN SCULPT: PREVIEW ONLY. Not saved, not exported, never in the game: no terrain writer exists.", 9.5, theme::EMBER)).wrap());
                ui.add(egui::Slider::new(&mut self.terrain_brush_radius, 5.0..=80.0).text("brush radius m"));
                ui.add(egui::Slider::new(&mut self.terrain_brush_strength, 0.5..=20.0).text("strength m"));
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Hill").on_hover_text("Raise a smooth hill at the last terrain point the cursor touched in the 3D view").clicked() {
                        self.terrain_dab(Brush::Raise);
                    }
                    if ui.button("Pit").clicked() {
                        self.terrain_dab(Brush::Lower);
                    }
                    if ui.button("Smooth").clicked() {
                        self.terrain_dab(Brush::Smooth);
                    }
                    if ui.button("Demo hills").on_hover_text("Raise a few hills and a pit around the camera target").clicked() {
                        self.terrain_demo();
                    }
                    if ui.add_enabled(self.terrain_patch.as_ref().map_or(false, |p| p.has_edits()), egui::Button::new("Reset sculpt")).clicked() {
                        if let Some(p) = self.terrain_patch.as_mut() {
                            p.reset();
                        }
                    }
                });
            }
        }
    }

    /// A few hills of different sizes and one pit around the patch centre (preview only).
    fn terrain_demo(&mut self) {
        use crate::place_terrain::Brush;
        if let Some(p) = self.terrain_patch.as_mut() {
            let (cx, cz) = (p.cx, p.cz);
            let n = [
                p.dab(Brush::Raise, cx - 45.0, cz - 20.0, 38.0, 14.0),
                p.dab(Brush::Raise, cx + 40.0, cz + 30.0, 30.0, 10.0),
                p.dab(Brush::Raise, cx + 5.0, cz + 70.0, 22.0, 7.0),
                p.dab(Brush::Lower, cx - 20.0, cz + 55.0, 18.0, 6.0),
            ];
            for _ in 0..2 {
                p.dab(Brush::Smooth, cx - 45.0, cz - 20.0, 38.0, 0.5);
            }
            self.status = Some((format!("terrain preview: demo hills and a pit ({} nodes changed). Preview only: not saved, not exported, not in the game", n.iter().sum::<usize>()), false));
        }
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        self.ensure_schema();
        self.terrain_panel(ui);
        let selected = self.selected();
        let (Some(s), Some(c), Some(i)) = (self.session.as_ref(), self.sel_container, self.sel_object) else {
            ui.label(theme::data_text("No object selected.", 10.5, theme::DIM));
            return;
        };
        let Some(o) = s.entry(c).and_then(|e| e.objects().get(i)) else { return };
        let pos_bytes = o.param(tags::POSITION).map(|d| d.to_vec()).unwrap_or_default();
        let positions = o.params_with_tag(tags::POSITION).len();
        if self.pos_for.as_ref().map_or(true, |(pc, pi, pb)| (*pc, *pi) != (c, i) || *pb != pos_bytes) {
            self.pos_edit = o.position().unwrap_or([0.0; 3]);
            self.pos_for = Some((c, i, pos_bytes.clone()));
        }
        let name_owned = s.object_name(o.tag()).unwrap_or_else(|| "(name not recovered)".to_string());
        let name = name_owned.as_str();
        ui.add_space(6.0);
        ui.label(theme::poster_text(name.rsplit('\\').next().unwrap_or(name), 18.0, theme::TX));
        ui.label(theme::data_text(format!("tag 0x{:08x}  ·  {name}", o.tag()), 10.0, theme::DIM));
        ui.label(theme::data_text(
            format!("container 0x{c:08x} {}  ·  from {}", short(editnodes::tag_name(c), c), s.source_of(c).unwrap_or("?")),
            10.0,
            theme::DIM,
        ));
        if let Some(edit) = &self.cell_edit {
            if let Some(list) = edit.containers.iter().position(|container| *container == c) {
                ui.add(egui::Label::new(theme::data_text(format!("cell {} · list {} · layer {:08x}\n{} · source SHA {}", edit.cell, list, edit.list_layer(list).unwrap_or(0), edit.source_rel, &edit.source_sha[..12]), 10.0, theme::EMBER)).wrap()).on_hover_text(&edit.source_sha);
            }
        }
        ui.label(theme::data_text(format!("Type {}   ClassName {}", o.type_name().unwrap_or_default(), o.class_name().unwrap_or_default()), 11.0, theme::TX));
        let class = o.class_name().unwrap_or_default();
        let present: std::collections::BTreeSet<u32> = o.params().iter().map(|p| p.tag()).collect();
        let mut open_picker = false;
        if ui.add_enabled(self.cell_edit.is_none(), egui::Button::new("Change ClassName…")).on_hover_text("EditNodes only. Cell staging supports placement transforms, duplicates and deletion, not class replacement.").clicked() {
            open_picker = true;
        }
        if selected.len() > 1 {
            ui.label(theme::data_text(
                format!("{} objects selected: the gizmo and nudges move all of them as one undo step; the fields below edit this one", selected.len()),
                10.0,
                theme::EMBER,
            ));
        }
        ui.add_space(4.0);
        let mut apply = false;
        let mut apply_offset = false;
        let mut revert = false;
        if positions == 1 {
            ui.horizontal_wrapped(|ui| {
                ui.label(theme::disp_text("POSITION", 11.0, theme::TX)).on_hover_text("Y is up. Each Apply is one undo step.");
                for (k, axis) in ["x", "y", "z"].iter().enumerate() {
                    ui.label(theme::data_text(*axis, 10.0, theme::FAINT));
                    ui.add(egui::DragValue::new(&mut self.pos_edit[k]).speed(0.05).max_decimals(4));
                }
                apply = theme::action_button(ui, "Apply move", true).clicked();
                revert = ui.button("Reset").clicked();
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(theme::data_text("offset", 10.0, theme::FAINT));
                for (k, axis) in ["dx", "dy", "dz"].iter().enumerate() {
                    ui.label(theme::data_text(*axis, 10.0, theme::FAINT));
                    ui.add(egui::DragValue::new(&mut self.off_edit[k]).speed(0.05).max_decimals(4).suffix(" m"));
                }
                apply_offset = ui.add_enabled(self.off_edit != [0.0; 3], egui::Button::new("Apply offset")).clicked();
            });
        } else {
            ui.label(theme::data_text(format!("{positions} Position params: moving is disabled for this object"), 10.0, theme::EMBER));
        }
        ui.add_space(8.0);
        ui.label(theme::disp_text(format!("PARAMETERS ({}, in stored order){}", o.params().len(), if self.show_info { "" } else { "  ·  hidden (I)" }), 11.0, theme::TX));
        let raw = o.params().iter().filter(|p| matches!(p.value(), Some(Value::Bytes(_)) | Some(Value::BadLua(_)))).count();
        if self.show_info {
            ui.label(theme::data_text(
                if raw == 0 { "fully decoded".to_string() } else { format!("{raw} param(s) kept as raw bytes (shown as sizes; never rewritten unless edited)") },
                9.5,
                theme::FAINT,
            ));
        }
        let mut lua_change: Option<(usize, LuaValue)> = None;
        let lua_buf = &mut self.lua_buf;
        egui::ScrollArea::vertical().id_source("pl_params").auto_shrink([false, false]).show(ui, |ui| {
            if !self.show_info {
                return;
            }
            egui::Grid::new("pl_param_grid").striped(true).num_columns(4).show(ui, |ui| {
                let mut seen: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
                for p in o.params() {
                    let n = seen.entry(p.tag()).or_default();
                    let tag_label = editnodes::tag_name(p.tag()).map(str::to_string).unwrap_or_else(|| format!("0x{:08x}", p.tag()));
                    let len = match p {
                        Node::Leaf { data, .. } => data.len(),
                        Node::Container { children, .. } => children.len(),
                    };
                    ui.label(theme::data_text(tag_label, 10.0, theme::TX));
                    ui.label(theme::data_text(format!("#{n}"), 10.0, theme::FAINT));
                    ui.label(theme::data_text(format!("{len} B"), 10.0, theme::FAINT));
                    match p.value() {
                        // Typed LuaParam editors; a change is one undo step (statically checked: not
                        // yet observed in game; a change of size is flagged).
                        Some(Value::Lua(l)) => {
                            let occ = *n;
                            ui.horizontal(|ui| {
                                ui.label(theme::data_text(format!("Lua {} =", l.name), 10.0, theme::DIM));
                                match &l.value {
                                    LuaValue::Bool(b) => {
                                        let mut v = *b != 0;
                                        if ui.checkbox(&mut v, "").changed() {
                                            lua_change = Some((occ, LuaValue::Bool(v as u8)));
                                        }
                                    }
                                    LuaValue::Int(x) => {
                                        let mut v = *x;
                                        if ui.add(egui::DragValue::new(&mut v)).changed() {
                                            lua_change = Some((occ, LuaValue::Int(v)));
                                        }
                                    }
                                    LuaValue::Float(x) => {
                                        let mut v = *x;
                                        if ui.add(egui::DragValue::new(&mut v).speed(0.05).max_decimals(4)).changed() {
                                            lua_change = Some((occ, LuaValue::Float(v)));
                                        }
                                    }
                                    LuaValue::Str(cur) => {
                                        let key = (c, i, occ);
                                        let buf = lua_buf.entry(key).or_insert_with(|| cur.clone());
                                        let r = ui.add(egui::TextEdit::singleline(buf).desired_width(160.0));
                                        if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) && buf != cur {
                                            lua_change = Some((occ, LuaValue::Str(buf.clone())));
                                        }
                                        if buf.len() != cur.len() {
                                            ui.label(theme::data_text("resize: statically checked", 9.0, theme::EMBER));
                                        }
                                    }
                                    LuaValue::Crc(h) => {
                                        let key = (c, i, occ);
                                        let cur = format!("{h:08x}");
                                        let buf = lua_buf.entry(key).or_insert_with(|| cur.clone());
                                        let r = ui.add(egui::TextEdit::singleline(buf).desired_width(80.0)).on_hover_text("crc (hex); Enter applies");
                                        if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) && *buf != cur {
                                            if let Ok(v) = u32::from_str_radix(buf.trim_start_matches("0x"), 16) {
                                                lua_change = Some((occ, LuaValue::Crc(v)));
                                            }
                                        }
                                    }
                                    LuaValue::CrcList(_) => {
                                        ui.label(theme::data_text(show_value(p.value()), 10.0, theme::DIM));
                                    }
                                }
                            });
                        }
                        v => {
                            ui.label(theme::data_text(show_value(v), 10.0, theme::DIM));
                        }
                    }
                    ui.end_row();
                    *n += 1;
                }
            });
        });
        if let Some((occ, value)) = lua_change {
            if let Some(s) = self.session.as_mut() {
                let r = s.set_lua_param(c, i, occ, value).map(|_| "LuaParam set (one undo step; statically checked: LuaParam edits are not yet observed in game)".to_string());
                self.lua_buf.clear();
                self.result(r);
            }
        }
        // The observed class schema: what this ClassName usually carries on this install. This is an
        // observation, not a format requirement; it is labelled as such and never blocks staging.
        if let Some(cs) = self.schema.as_ref().and_then(|s| s.class(&class)) {
            ui.add_space(6.0);
            ui.label(theme::disp_text(format!("CLASS SCHEMA · observed on {} object(s)", cs.objects), 10.5, theme::TX));
            let mut names: Vec<String> = cs
                .params
                .iter()
                .map(|(tag, n)| format!("{} ×{n}", editnodes::tag_name(*tag).map(str::to_string).unwrap_or_else(|| format!("0x{tag:08x}"))))
                .collect();
            let more = names.len().saturating_sub(12);
            names.truncate(12);
            let mut text = format!("observed: {}", names.join(", "));
            if more > 0 {
                text.push_str(&format!(", … {more} more"));
            }
            ui.add(egui::Label::new(theme::data_text(text, 9.5, theme::DIM)).wrap());
            let common: Vec<String> = cs.common().iter().map(|t| editnodes::tag_name(*t).unwrap_or("param").to_string()).collect();
            if !common.is_empty() {
                ui.add(egui::Label::new(theme::data_text(format!("on every sample: {}", common.join(", ")), 9.0, theme::FAINT)).truncate()).on_hover_text(common.join(", "));
            }
            let unexpected: Vec<String> = present.iter().filter(|t| !cs.params.contains_key(t)).map(|t| editnodes::tag_name(*t).unwrap_or("param").to_string()).collect();
            if !unexpected.is_empty() {
                ui.add(egui::Label::new(theme::data_text(format!("not observed on this class: {}", unexpected.join(", ")), 9.5, theme::EMBER)).wrap());
            }
            if cs.objects < 5 {
                ui.label(theme::data_text("few samples: a hint only", 9.0, theme::FAINT));
            }
        }
        if revert {
            self.pos_for = None;
        }
        if apply {
            self.apply_move();
        }
        if apply_offset {
            self.apply_offset();
        }
        if open_picker {
            self.class_picker = Some(String::new());
        }
    }

    /// The selected objects' current nodes, with their containers.
    fn selected_nodes(&self) -> Vec<(u32, Node)> {
        let Some(s) = self.session.as_ref() else { return Vec::new() };
        self.selected().into_iter().filter_map(|(c, i)| Some((c, s.entry(c)?.objects().get(i)?.clone()))).collect()
    }

    /// Ctrl+C (Ariane): copy the selection.
    fn copy(&mut self, ctx: &egui::Context) {
        let nodes = self.selected_nodes();
        if nodes.is_empty() {
            return;
        }
        let n = nodes.len();
        self.clipboard = nodes;
        self.clip_cut = false;
        self.view.toast(ctx, format!("Copied {n} | Ctrl+V: paste at the camera target | Ctrl+Shift+V: paste in place"));
    }

    /// Ctrl+X (Ariane): copy, then delete, as one undo step; pasting restores the original tags.
    fn cut(&mut self, ctx: &egui::Context) {
        let nodes = self.selected_nodes();
        if nodes.is_empty() {
            return;
        }
        let sel = self.selected();
        let Some(s) = self.session.as_mut() else { return };
        match s.remove_objects(&sel) {
            Ok(n) => {
                self.clipboard = nodes;
                self.clip_cut = true;
                self.set_selection(Vec::new());
                self.view.toast(ctx, format!("Cut {n}"));
                self.result(Ok(format!("cut {n} object(s)")));
            }
            Err(e) => self.result(Err(e)),
        }
    }

    /// Ctrl+V / Ctrl+Shift+V (Ariane): paste the clipboard at the camera target, or in place.
    /// Each object goes to the end of its source container. A copy gets a new tag (`name_N` when
    /// the names are known); a cut's object keeps its original tag while it is free.
    fn paste(&mut self, ctx: &egui::Context, in_place: bool) {
        if self.clipboard.is_empty() {
            return;
        }
        let offset = if in_place {
            [0.0; 3]
        } else {
            let ps: Vec<[f32; 3]> = self.clipboard.iter().filter_map(|(_, n)| n.position()).collect();
            let n = ps.len().max(1) as f32;
            let c = ps.iter().fold([0.0; 3], |a, p| [a[0] + p[0] / n, a[1] + p[1] / n, a[2] + p[2] / n]);
            let t = self.view.target();
            [t[0] - c[0], t[1] - c[1], t[2] - c[2]]
        };
        let label = if in_place { "Paste in place" } else { "Paste" };
        self.insert_copies(ctx, self.clipboard.clone(), offset, self.clip_cut, label);
    }

    /// Ctrl+D: duplicate the selection one snap step along X.
    fn duplicate(&mut self, ctx: &egui::Context) {
        let nodes = self.selected_nodes();
        if nodes.is_empty() {
            return;
        }
        let step = self.view.snap;
        self.insert_copies(ctx, nodes, [step, 0.0, 0.0], false, "Duplicate");
    }

    fn insert_copies(&mut self, ctx: &egui::Context, nodes: Vec<(u32, Node)>, offset: [f32; 3], keep_tags: bool, label: &str) {
        // Terrain-cell mode: objects copied in ANOTHER cell (their list does not exist here) are pasted into
        // this cell's base-layer list, so guards, vehicles, towers etc. can be brought in from any base cell.
        let base_list = self.cell_edit.as_ref().and_then(|e| (0..e.containers.len()).find(|&li| e.list_layer(li) == Some(0)).map(|li| e.containers[li]));
        let nodes: Vec<(u32, Node)> = match (self.session.as_ref(), base_list) {
            (Some(s), Some(base)) => nodes.into_iter().map(|(c, n)| if s.entry(c).is_none() { (base, n) } else { (c, n) }).collect(),
            _ => nodes,
        };
        let Some(s) = self.session.as_mut() else { return };
        let mut items = Vec::new();
        let mut names = Vec::new();
        let mut minted: Vec<(u32, String)> = Vec::new();
        // LuaParams holding a crc or crc list may name other objects. References are never rewritten
        // on paste (Hammer rewrites any field equal to an old name; CP77 copies ids and finds the
        // clash only at export), so the user is told which copies carry them.
        let refs = nodes
            .iter()
            .filter(|(_, n)| {
                n.params().iter().any(|p| matches!(p.value(), Some(Value::Lua(l)) if matches!(l.value, LuaValue::Crc(_) | LuaValue::CrcList(_))))
            })
            .count();
        let mut taken: Vec<(u32, u32)> = Vec::new();
        let mut ends: std::collections::BTreeMap<u32, usize> = std::collections::BTreeMap::new();
        for (c, node) in nodes {
            let Some(entry) = s.entry(c) else { continue };
            let mut n = node.clone();
            let tag_free = !entry.objects().iter().any(|o| o.tag() == node.tag()) && !taken.contains(&(c, node.tag()));
            if !(keep_tags && tag_free) {
                let (mut t, mut name) = s.mint_tag(c, node.tag());
                // Several copies of one object in one paste: mint past the ones already chosen.
                let mut k = 0;
                while taken.contains(&(c, t)) && k < 1000 {
                    let alt = sab_formats::pandemic_hash(&format!("sab_places_copy\\{c:08x}\\{:08x}\\p{k}", node.tag()));
                    t = alt;
                    name = None;
                    k += 1;
                }
                n.set_tag(t);
                if let Some(nm) = &name {
                    minted.push((t, nm.clone()));
                }
                names.push(name.unwrap_or_else(|| format!("0x{t:08x} (not addressable by name)")));
            }
            taken.push((c, n.tag()));
            if let Some(p) = n.position() {
                if offset != [0.0; 3] {
                    let _ = n.set_position([p[0] + offset[0], p[1] + offset[1], p[2] + offset[2]]);
                }
            }
            let end = ends.entry(c).or_insert_with(|| entry.objects().len());
            items.push((c, *end, n));
            *end += 1;
        }
        let count = items.len();
        match s.insert_objects(items, &format!("{label} {count}")) {
            Ok(placed) => {
                for (t, nm) in &minted {
                    s.note_name(*t, nm);
                }
                self.set_selection(placed);
                let named = if names.is_empty() { String::new() } else { format!("; new names: {}", names.join(", ")) };
                self.view.toast(ctx, format!("{label}: {count} object(s)"));
                if refs > 0 {
                    self.view.toast(ctx, format!("{refs} copied object(s) carry crc LuaParams that may name other objects; those references were NOT changed"));
                }
                self.result(Ok(format!("{label}: {count} object(s){named}")));
            }
            Err(e) => {
                self.view.toast(ctx, format!("{label} refused: {e}"));
                self.result(Err(e));
            }
        }
    }

    /// Delete / Backspace (Ariane): remove the selection as one undo step.
    fn delete(&mut self, ctx: &egui::Context) {
        let sel = self.selected();
        if sel.is_empty() {
            return;
        }
        let Some(s) = self.session.as_mut() else { return };
        let r = s.remove_objects(&sel).map(|n| format!("deleted {n} object(s) (one undo step)"));
        if r.is_ok() {
            self.set_selection(Vec::new());
            self.view.toast(ctx, "Deleted (Ctrl+Z restores)");
        }
        self.result(r);
    }

    /// Several Position edits as one undo step (a gizmo drag or a nudge of the selection).
    fn move_many(&mut self, moves: &[(u32, usize, [f32; 3])]) {
        let Some(s) = self.session.as_mut() else { return };
        let r = match moves {
            [(c, i, pos)] => s.move_object(*c, *i, *pos).map(|_| format!("moved object {i} of 0x{c:08x} to ({:.3}, {:.3}, {:.3})", pos[0], pos[1], pos[2])),
            _ => s.move_objects(moves).map(|n| format!("moved {n} objects (one undo step)")),
        };
        self.result(r);
    }

    /// A finished gizmo rotation (or a test's equivalent): rotate the objects by `r` as one undo step. A single object turns
    /// about its own origin; several turn about `pivot`. This is the one code path the viewport's Rotate action uses.
    fn rotate_many(&mut self, ctx: &egui::Context, objs: &[ObjRef], r: [[f32; 3]; 3], pivot: [f32; 3]) {
        let Some(s) = self.session.as_mut() else { return };
        let res = s.rotate_objects(objs, r, (objs.len() > 1).then_some(pivot)).map(|n| format!("rotated {n} object(s) (one undo step)"));
        if let Err(e) = &res {
            self.view.toast(ctx, format!("Rotate refused: {e}"));
        }
        self.result(res);
    }

    /// The "Apply move" action: one undoable edit of the selected object's Position.
    fn apply_move(&mut self) {
        let (Some(c), Some(i)) = (self.sel_container, self.sel_object) else { return };
        self.move_to(c, i, self.pos_edit);
    }

    /// The "Apply offset" action: the stored Position plus the typed offset, as one undoable edit.
    fn apply_offset(&mut self) {
        let (Some(c), Some(i)) = (self.sel_container, self.sel_object) else { return };
        let Some(p) = self.session.as_ref().and_then(|s| s.entry(c)?.objects().get(i)?.position()) else { return };
        let d = self.off_edit;
        self.move_to(c, i, [p[0] + d[0], p[1] + d[1], p[2] + d[2]]);
    }

    /// One undoable Position edit (Apply move, Apply offset, a nudge, or one finished gizmo drag).
    fn move_to(&mut self, c: u32, i: usize, pos: [f32; 3]) {
        let Some(s) = self.session.as_mut() else { return };
        let r = s.move_object(c, i, pos).map(|_| format!("moved object {i} to ({:.3}, {:.3}, {:.3})", pos[0], pos[1], pos[2]));
        self.result(r);
    }

    /// Undo one step and select its objects again (CP77 / Hammer restore the selection with the
    /// step). Returns the step's name.
    fn undo(&mut self) -> Option<String> {
        let before = self.selection_tags();
        let s = self.session.as_mut()?;
        let (_, cursor) = s.journal();
        if cursor == 0 {
            self.result(Ok("nothing to undo".into()));
            return None;
        }
        let g = s.groups()[cursor - 1];
        let name = s.group_name(g).to_string();
        let r = s.undo().map(|_| format!("undone: {name}"));
        let ok = r.is_ok();
        self.result(r);
        if ok {
            self.remap_selection(&before);
            self.select_group(g);
        }
        Some(name)
    }

    fn redo(&mut self) -> Option<String> {
        let before = self.selection_tags();
        let s = self.session.as_mut()?;
        let (j, cursor) = s.journal();
        if cursor >= j.len() {
            self.result(Ok("nothing to redo".into()));
            return None;
        }
        let g = s.groups()[cursor];
        let name = s.group_name(g).to_string();
        let r = s.redo().map(|_| format!("redone: {name}"));
        let ok = r.is_ok();
        self.result(r);
        if ok {
            self.remap_selection(&before);
            self.select_group(g);
        }
        Some(name)
    }

    /// The selection as `(object, tag)`, taken before a step that may shift object indices.
    fn selection_tags(&self) -> Vec<(ObjRef, u32)> {
        let Some(s) = self.session.as_ref() else { return Vec::new() };
        self.selected().into_iter().filter_map(|r| Some((r, s.entry(r.0)?.objects().get(r.1)?.tag()))).collect()
    }

    /// Re-point a selection taken by [`selection_tags`] at the same objects after an undo or redo:
    /// each keeps its tag, the nearest object with that tag in its container is used when the index
    /// moved, and objects that no longer exist drop out of the selection.
    fn remap_selection(&mut self, before: &[(ObjRef, u32)]) {
        let Some(s) = self.session.as_ref() else { return };
        let mut now: Vec<ObjRef> = Vec::new();
        for &((c, i), tag) in before {
            let Some(objs) = s.entry(c).map(|e| e.objects()) else { continue };
            let found = if objs.get(i).map_or(false, |o| o.tag() == tag) {
                Some(i)
            } else {
                (0..objs.len()).filter(|&k| objs[k].tag() == tag).min_by_key(|&k| k.abs_diff(i))
            };
            if let Some(k) = found {
                if !now.contains(&(c, k)) {
                    now.push((c, k));
                }
            }
        }
        self.set_selection(now);
    }

    /// Select the objects of undo step `g` that exist in the current state.
    fn select_group(&mut self, g: u64) {
        let Some(s) = self.session.as_ref() else { return };
        let (j, _) = s.journal();
        let mut objs: Vec<ObjRef> = Vec::new();
        // Removed objects count too: after undoing a delete they exist again and are reselected; the
        // tag check skips whatever does not exist in the current state.
        for (e, gg) in j.iter().zip(s.groups()) {
            if *gg != g {
                continue;
            }
            let r = (e.container(), e.object_index());
            let exists = s.entry(r.0).and_then(|en| en.objects().get(r.1)).map_or(false, |o| o.tag() == e.object_tag());
            if exists && !objs.contains(&r) {
                objs.push(r);
            }
        }
        if !objs.is_empty() {
            self.set_selection(objs);
        }
    }

    /// Ctrl+Shift+E (Ariane): the selection as a prefab file, positions relative to its centre.
    fn export_prefab_to(&mut self, p: &Path) -> Result<usize, String> {
        let nodes = self.selected_nodes();
        if nodes.is_empty() {
            return Err("nothing selected".into());
        }
        if let Some(s) = self.session.as_ref() {
            sab_tools::stage::guard_write(p, &s.game)?;
        }
        let ps: Vec<[f32; 3]> = nodes.iter().filter_map(|(_, n)| n.position()).collect();
        let k = ps.len().max(1) as f32;
        let centre = ps.iter().fold([0.0; 3], |a, q| [a[0] + q[0] / k, a[1] + q[1] / k, a[2] + q[2] / k]);
        let objects: Vec<serde_json::Value> = nodes
            .iter()
            .map(|(c, n)| {
                let mut rel = n.clone();
                if let Some(q) = n.position() {
                    let _ = rel.set_position([q[0] - centre[0], q[1] - centre[1], q[2] - centre[2]]);
                }
                serde_json::json!({ "container": format!("{c:08x}"), "node": sab_tools::editnodes_stage::node_json(&rel) })
            })
            .collect();
        let v = serde_json::json!({ "format": "sab_places_prefab", "version": 1, "objects": objects });
        std::fs::write(p, serde_json::to_string_pretty(&v).unwrap()).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(nodes.len())
    }

    /// Ctrl+Shift+I (Ariane): place a prefab at the camera target, with new tags, as one step.
    fn import_prefab_from(&mut self, ctx: &egui::Context, p: &Path) -> Result<(), String> {
        let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?;
        if v["format"] != "sab_places_prefab" {
            return Err("not a sab_places_prefab file".into());
        }
        let mut nodes = Vec::new();
        for o in v["objects"].as_array().ok_or("prefab has no objects")? {
            let c = u32::from_str_radix(o["container"].as_str().ok_or("object without container")?, 16).map_err(|e| e.to_string())?;
            nodes.push((c, sab_tools::editnodes_stage::node_from_json(&o["node"])?));
        }
        let t = self.view.target();
        self.insert_copies(ctx, nodes, t, false, "Prefab");
        Ok(())
    }

    /// Detail: the journal, the project path, and the last stage report.
    fn side(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(theme::disp_text("CHANGES", 12.0, theme::TX));
            if ui.button("⌨ Shortcuts (?)").clicked() {
                self.show_shortcuts = !self.show_shortcuts;
            }
        });
        let Some(s) = self.session.as_ref() else {
            if ui.button("Open project…").clicked() {
                self.open_project();
            }
            return;
        };
        let rows = change_rows(s);
        let mut jump: Option<Vec<ObjRef>> = None;
        egui::ScrollArea::vertical().id_source("pl_journal").max_height(200.0).auto_shrink([false, true]).show(ui, |ui| {
            if rows.is_empty() {
                ui.label(theme::data_text("no edits yet", 10.0, theme::FAINT));
            }
            for row in &rows {
                let mark = if row.active { "✔" } else { "–" };
                let check = if row.demonstrated { "  · kind demonstrated on locators" } else { "  · not demonstrated in game" };
                let text = format!("{mark} {} ({} edit{}){check}", row.name, row.edits, if row.edits == 1 { "" } else { "s" });
                let colour = if !row.active { theme::FAINT } else if row.demonstrated { theme::TX } else { theme::EMBER };
                let r = ui.add(egui::Label::new(theme::data_text(text, 10.0, colour)).sense(egui::Sense::click()));
                let evidence = if row.demonstrated {
                    "Evidence: this kind of operation was demonstrated in game on always-loaded locators (E1, Gate A). This particular edit is not verified."
                } else {
                    "Evidence: this kind of edit has never been demonstrated in game; statically checked only."
                };
                if r.on_hover_text(format!("{}\n{evidence}\nclick: select these objects and jump to them", row.detail)).clicked() {
                    jump = Some(row.objects.clone());
                }
            }
        });
        if let Some(objs) = jump {
            if !objs.is_empty() {
                self.set_selection(objs);
                self.view.request_focus();
            }
        }
        // Checks (Hammer's map check, CP77's export checks): errors block staging; click to go to.
        let issues = self.session.as_ref().map(|s| s.validate()).unwrap_or_default();
        ui.separator();
        ui.label(theme::disp_text(format!("CHECKS ({})", issues.len()), 11.0, theme::TX));
        let mut goto = None;
        if issues.is_empty() {
            ui.label(theme::data_text("no issues", 10.0, theme::FAINT));
        }
        for is in &issues {
            let (tag, colour) = match is.severity {
                Severity::Error => ("error", theme::RED),
                Severity::Warning => ("warning", theme::EMBER),
                Severity::Note => ("note", theme::DIM),
            };
            let r = ui.add(egui::Label::new(theme::data_text(format!("{tag}: {}", is.message), 9.5, colour)).sense(egui::Sense::click()));
            if let Some(at) = is.at {
                if r.on_hover_text("click: go to the object").clicked() {
                    goto = Some(at);
                }
            }
        }
        if let Some(at) = goto {
            self.set_selection(vec![at]);
            self.view.request_focus();
        }
        // B1 live readback: one request, explicit state, editor vs game side by side.
        ui.separator();
        ui.label(theme::disp_text("LIVE READBACK (B1)", 11.0, theme::TX));
        let can_read = self.session.is_some() && self.primary().is_some();
        if ui
            .add_enabled(can_read, egui::Button::new("Read selection from game"))
            .on_hover_text("Writes one request to sab_cmd.lua; the game (with sab_asi.asi) polls it and logs the live positions. sab_cmd.lua is restored afterwards.")
            .clicked()
        {
            self.live_read();
        }
        if self.live_missing > 0 {
            ui.label(theme::data_text(format!("{} selected object(s) not queried: no recovered Lua name", self.live_missing), 9.0, theme::EMBER));
        }
        let status = self.live.status().clone();
        match &status {
            LiveStatus::Idle => {
                ui.label(theme::data_text("no request yet. Needs the game running with sab_asi.asi and a save loaded.", 9.5, theme::FAINT));
            }
            LiveStatus::Waiting => {
                ui.label(theme::data_text("waiting for the game to run sab_cmd.lua (polled every 100 ms)…", 9.5, theme::COLD));
            }
            LiveStatus::TimedOut => {
                ui.label(theme::data_text("no reply within 20 s: is the game running with the dev sab_asi.asi, and a save loaded?", 9.5, theme::RED));
            }
            LiveStatus::Error(e) => {
                ui.add(egui::Label::new(theme::data_text(format!("error: {e}"), 9.5, theme::RED)).wrap());
            }
            LiveStatus::Ready(r) => {
                let request = self.live.request().cloned();
                let stale = self.session.as_ref().map_or(true, |s| request.as_ref().map_or(true, |q| q.revision != s.revision()));
                if stale {
                    ui.label(theme::data_text("STALE: the editor changed after this request; positions below are not a current comparison. Read again.", 9.0, theme::RED));
                }
                if let Some(req) = request {
                    if let Some(n) = r.node_count {
                        ui.label(theme::data_text(format!("game node objects: {n}"), 9.0, theme::DIM));
                    }
                    if let Some(e) = &r.node_error {
                        ui.add(egui::Label::new(theme::data_text(format!("game node lookup: {e}"), 9.0, theme::EMBER)).wrap());
                    }
                    for f in &r.fatals {
                        ui.add(egui::Label::new(theme::data_text(format!("FATAL: {f}"), 9.0, theme::RED)).wrap());
                    }
                    egui::Grid::new("pl_live_rows").num_columns(1).striped(true).show(ui, |ui| {
                        for (k, q) in req.objects.iter().enumerate() {
                            let short = q.path.rsplit('\\').next().unwrap_or(&q.path).to_string();
                            let reading = r.readings.get(k).and_then(|x| x.as_ref());
                            let (game, colour) = match reading {
                                Some(Ok(p)) => {
                                    let d = [p[0] - q.editor_pos[0], p[1] - q.editor_pos[1], p[2] - q.editor_pos[2]];
                                    let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                                    (format!("({:.3}, {:.3}, {:.3}) Δ {dist:.3}", p[0], p[1], p[2]), if dist < 0.01 { theme::COLD } else { theme::EMBER })
                                }
                                Some(Err(e)) => (format!("error: {e}"), theme::RED),
                                None => ("no reply for this object".to_string(), theme::FAINT),
                            };
                            let editor = format!("({:.3}, {:.3}, {:.3})", q.editor_pos[0], q.editor_pos[1], q.editor_pos[2]);
                            let row = format!("{short}\neditor {editor}\ngame {game}");
                            ui.add(egui::Label::new(theme::data_text(row.clone(), 9.0, colour)).truncate()).on_hover_text(format!("{}\neditor {editor}\ngame {game}", q.path));
                            ui.end_row();
                        }
                    });
                }
            }
        }
        if self.live.has_backup() {
            if ui.button("Restore sab_cmd.lua").on_hover_text("Put the game's command file back exactly as it was before this editor wrote a request").clicked() {
                self.live.restore();
                self.status = Some(("live readback: sab_cmd.lua restored".into(), false));
            }
        }
        ui.separator();
        ui.label(theme::disp_text("PROJECT", 11.0, theme::TX));
        ui.label(theme::data_text(self.project.as_ref().map_or("not saved".to_string(), |p| p.display().to_string()), 9.5, theme::DIM));
        ui.separator();
        ui.label(theme::disp_text("STAGE", 11.0, theme::TX));
        ui.label(theme::data_text(
            "\"Stage mod\" writes DLC\\01\\France\\EditNodes\\EditNodes.pack plus install.ps1 / restore.ps1 to a \
             new folder under the export directory. Nothing is installed: run install.ps1 yourself with the game closed.",
            9.5,
            theme::FAINT,
        ));
        ui.label(theme::data_text(format!("Staged mods and the default project folder: {}", self.export_dir), 9.5, theme::DIM));
        if ui.button("Choose export folder…").clicked() {
            self.want_export_dir = true;
        }
        if let Some(dir) = &self.staged_dir {
            ui.add_space(4.0);
            ui.label(theme::disp_text("STAGED PACKAGE", 10.5, theme::TX));
            ui.add(egui::Label::new(theme::data_text(dir.display().to_string(), 9.0, theme::DIM)).wrap());
            let files: Vec<String> = std::fs::read_dir(dir)
                .map(|rd| rd.filter_map(|e| e.ok()).map(|e| format!("{} ({})", e.file_name().to_string_lossy(), e.metadata().map(|m| m.len()).unwrap_or(0))).collect())
                .unwrap_or_default();
            ui.add(egui::Label::new(theme::data_text(files.join(", "), 9.0, theme::FAINT)).wrap());
            ui.horizontal_wrapped(|ui| {
                if ui.button("Open folder").clicked() {
                    #[cfg(windows)]
                    let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
                }
                let install = dir.join("install.ps1");
                if ui.button("Copy install command").on_hover_text("Run it with the game closed; restore.ps1 undoes the install").clicked() {
                    let cmd = format!("powershell -ExecutionPolicy Bypass -File \"{}\"", install.display());
                    ui.output_mut(|o| o.copied_text = cmd.clone());
                    self.status = Some((format!("copied: {cmd}"), false));
                }
            });
        }
        ui.label(theme::data_text("Terrain-cell (patch megapack) export: disabled, unverified in game (M0 E2).", 9.5, theme::EMBER));
        if let Some(rep) = &self.report {
            egui::ScrollArea::vertical().id_source("pl_report").auto_shrink([false, false]).show(ui, |ui| {
                ui.label(theme::data_text(rep, 9.5, theme::DIM));
            });
        }
    }

    fn save_project(&mut self, export_dir: &str) {
        #[cfg(test)]
        if let Some(p) = self.test_dialog.take() {
            self.save_project_to(&p);
            return;
        }
        let mut d = rfd::FileDialog::new().set_title("Save placement project").add_filter("placement project", &["json"]).set_file_name("placements.sabproj.json");
        // Start in the export folder (created when missing and allowed), never in whatever folder the
        // system dialog last used, which may be the game folder.
        if !export_dir.is_empty() && !Path::new(export_dir).is_dir() && self.session.as_ref().map_or(false, |s| sab_tools::stage::guard_write(Path::new(export_dir), &s.game).is_ok()) {
            let _ = std::fs::create_dir_all(export_dir);
        }
        if Path::new(export_dir).is_dir() {
            d = d.set_directory(export_dir);
        }
        if let Some(p) = d.save_file() {
            self.save_project_to(&p);
        }
    }

    fn save_project_to(&mut self, p: &Path) {
        if let (Some(edit), Some(s)) = (self.cell_edit.as_ref(), self.session.as_ref()) {
            let r = sab_tools::cellsession::save_cell_project(edit, s, p).map(|_| format!("saved terrain-cell project {}", p.display())).map_err(|e| format!("could not save the project: {e}"));
            if r.is_ok() {
                self.project = Some(p.to_path_buf());
                self.saved_rev = s.revision();
            }
            self.result(r);
            return;
        }
        let Some(s) = self.session.as_ref() else { return };
        let r = s.save(p).map(|_| format!("saved {}", p.display())).map_err(|e| format!("could not save the project: {e}. Choose another folder, for example one in Documents."));
        if r.is_ok() {
            self.project = Some(p.to_path_buf());
            self.saved_rev = s.revision();
        }
        self.result(r);
    }

    fn open_project(&mut self) {
        #[cfg(test)]
        if let Some(p) = self.test_dialog.take() {
            self.open_project_from(&p);
            return;
        }
        if !self.confirm_discard("Opening another project replaces the current edits.") {
            return;
        }
        let mut d = rfd::FileDialog::new().set_title("Open placement project").add_filter("placement project", &["json"]);
        let start = self.project.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(&self.export_dir));
        if start.is_dir() {
            d = d.set_directory(start);
        }
        if let Some(p) = d.pick_file() {
            self.open_project_from(&p);
        }
    }

    /// Open a project file without the file dialog (start-up flag `--places-project`).
    pub fn open_project_path(&mut self, p: &Path) {
        self.open_project_from(p);
    }

    fn open_project_from(&mut self, p: &Path) {
        if sab_tools::cellsession::cell_project_id(p).is_some() {
            // a terrain-cell project: experimental, flag-gated, reopened over the rebuilt cell sources
            if !self.cells_enabled {
                self.status = Some(("this is a terrain-cell project; terrain-cell editing is experimental and off: set SAB_WORKSHOP_TERRAIN_CELLS=1 to open it".into(), true));
                return;
            }
            match sab_tools::cellsession::open_cell_project(p) {
                Ok((edit, s)) => {
                    let game_dir = s.game.display().to_string();
                    let cell = edit.cell;
                    self.set_session(s);
                    self.game_dir = game_dir;
                    self.cell_input = cell.to_string();
                    self.status = Some((format!("opened terrain-cell project {} (cell {})", p.display(), cell), false));
                    self.cell_edit = Some(edit);
                    self.project = Some(p.to_path_buf());
                    self.focus_cell_objects();
                }
                Err(e) => self.status = Some((format!("could not open the terrain-cell project: {e}"), true)),
            }
            return;
        }
        match Session::load(p) {
            Ok(s) => {
                let game_dir = s.game.display().to_string();
                self.set_session(s);
                self.game_dir = game_dir;
                self.status = Some((format!("opened {}", p.display()), false));
                self.project = Some(p.to_path_buf());
            }
            Err(e) => self.status = Some((format!("could not open project: {e}"), true)),
        }
    }

    /// Run the page's own actions in sequence inside the live app (`--places-autotest <dir>`): select,
    /// move +3 m on X, undo, redo, save the project, reopen it, stage. One step per ~1.5 s so each state
    /// is on screen; every step's state is appended to `<dir>/autotest_log.txt`. Input devices are not
    /// involved: this exercises the page logic and rendering, not mouse delivery.
    pub fn start_autotest(&mut self, dir: PathBuf) {
        let _ = std::fs::create_dir_all(&dir);
        self.autotest = Some(Autotest { dir, step: 0, next: std::time::Instant::now() + std::time::Duration::from_secs(2), log: Vec::new(), orig: None });
    }

    fn autotest_tick(&mut self, ctx: &egui::Context) {
        let Some(mut a) = self.autotest.take() else { return };
        ctx.request_repaint();
        if std::time::Instant::now() < a.next {
            self.autotest = Some(a);
            return;
        }
        let state = |p: &Places| -> String {
            let pos = (|| Some(p.session.as_ref()?.entry(p.sel_container?)?.objects().get(p.sel_object?)?.position()?))();
            let (j, c) = p.session.as_ref().map(|s| (s.journal().0.len(), s.journal().1)).unwrap_or((0, 0));
            format!("position {pos:?}; journal {j}, active {c}; status {:?}", p.status.as_ref().map(|s| &s.0))
        };
        let label = match a.step {
            0 => {
                if self.sel_object.is_none() {
                    a.next = std::time::Instant::now() + std::time::Duration::from_millis(500);
                    self.autotest = Some(a);
                    return;
                }
                a.orig = (|| self.session.as_ref()?.entry(self.sel_container?)?.objects().get(self.sel_object?)?.position())();
                "1 select"
            }
            1 => {
                if let Some(o) = a.orig {
                    self.pos_edit = [o[0] + 3.0, o[1], o[2]];
                }
                self.apply_move();
                "2 apply move (+3 m x)"
            }
            2 => {
                self.undo();
                "3 undo"
            }
            3 => {
                self.redo();
                "4 redo"
            }
            4 => {
                self.save_project_to(&a.dir.join("autotest.sabproj.json"));
                "5 save project"
            }
            5 => {
                let (c, i) = (self.sel_container, self.sel_object);
                self.open_project_from(&a.dir.join("autotest.sabproj.json"));
                self.sel_container = c;
                self.sel_object = i;
                "6 reopen project"
            }
            6 => {
                self.stage(&a.dir.to_string_lossy());
                "7 stage"
            }
            _ => {
                a.log.push(format!("report:\n{}", self.report.clone().unwrap_or_default()));
                let _ = std::fs::write(a.dir.join("autotest_log.txt"), a.log.join("\n"));
                self.status = Some(("AUTOTEST DONE".into(), false));
                return;
            }
        };
        a.log.push(format!("{label}: {}", state(self)));
        let _ = std::fs::write(a.dir.join("autotest_log.txt"), a.log.join("\n"));
        a.step += 1;
        a.next = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        self.autotest = Some(a);
    }
}

#[cfg(test)]
#[path = "place_cell_authoring_tests.rs"]
mod cell_authoring_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use sab_formats::editnodes::{Entry, Pack, Root};
    use sab_tools::stage::EditNodesSources;

    fn object(tag: u32, pos: [f32; 3]) -> Node {
        Node::Container {
            tag,
            children: vec![
                Node::Leaf { tag: tags::CLASS_NAME, data: b"Locator\0".to_vec() },
                Node::Leaf { tag: tags::POSITION, data: pos.iter().flat_map(|v| v.to_le_bytes()).collect() },
                Node::Leaf { tag: tags::LUA_PARAM, data: b"bFlag\0\x31\x4f\x44\x1b\x01".to_vec() },
            ],
        }
    }

    fn session() -> Session {
        let base = Pack { entries: vec![Entry { hash: 0x10, root: Root::EditNode { objects: vec![object(0xA, [1.0, 2.0, 3.0]), object(0xB, [4.0, 5.0, 6.0])] } }] };
        let dlc = Pack { entries: vec![Entry { hash: 0x20, root: Root::EditNode { objects: vec![object(0xC, [7.0, 8.0, 9.0])] } }] };
        Session::from_sources(Path::new("G"), EditNodesSources::from_bytes(base.write(), dlc.write(), "LF".into()).unwrap())
    }

    fn frame(ctx: &egui::Context, p: &mut Places, out: &str) {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0))), ..Default::default() };
        let _ = ctx.run(input, |ctx| p.show(ctx, "", out));
    }

    fn pos(p: &Places) -> [f32; 3] {
        p.session.as_ref().unwrap().entry(0x10).unwrap().objects()[1].position().unwrap()
    }

    #[test]
    fn cell_target_requires_the_exact_list_tag_class_position_and_layer() {
        let session = session();
        let target = crate::cell_props::CellTarget { cell: 1, list: 0, tag: 0xB, class_name: "Locator".into(), pos: [4.0, 5.0, 6.0], layer: 0 };
        assert_eq!(cell_target_object(&session, 0x10, 0, &target).unwrap(), (0x10, 1));
        assert!(cell_target_object(&session, 0x20, 0, &target).is_err());
        assert!(cell_target_object(&session, 0x30, 0, &target).is_err());
        assert!(cell_target_object(&session, 0x10, 1, &target).is_err());
        let mut stale = target.clone();
        stale.pos[0] += 1.0;
        assert!(cell_target_object(&session, 0x10, 0, &stale).is_err());
        stale = target.clone();
        stale.class_name = "Other".into();
        assert!(cell_target_object(&session, 0x10, 0, &stale).is_err());
        let duplicate = Pack { entries: vec![Entry { hash: 0x10, root: Root::EditNode { objects: vec![object(0xB, target.pos), object(0xB, target.pos)] } }] };
        let duplicate_session = Session::from_sources(Path::new("G"), EditNodesSources::from_bytes(duplicate.write(), Pack { entries: vec![] }.write(), "test".into()).unwrap());
        assert!(cell_target_object(&duplicate_session, 0x10, 0, &target).unwrap_err().contains("ambiguous"));
    }

    #[test]
    fn refused_cell_target_preserves_dirty_session_selection_and_project() {
        let mut page = Places::new();
        page.set_session(session());
        page.pick(Some((0x10, 1)), PickMode::Replace);
        page.move_many(&[(0x10, 1, [8.0, 9.0, 10.0])]);
        page.project = Some(PathBuf::from("important.sabproject.json"));
        let before = page.session.as_ref().unwrap().to_project_json();
        let target = crate::cell_props::CellTarget { cell: 1, list: 0, tag: 0xB, class_name: "Locator".into(), pos: [4.0, 5.0, 6.0], layer: 0 };
        for enabled in [false, true] {
            page.cells_enabled = enabled;
            page.open_cell_target("missing-game-for-cell-target-test", target.clone());
            assert!(page.status.as_ref().unwrap().1);
            assert_eq!(page.session.as_ref().unwrap().to_project_json(), before);
            assert_eq!(page.primary(), Some((0x10, 1)));
            assert_eq!(page.project.as_deref(), Some(Path::new("important.sabproject.json")));
            assert!(page.is_dirty());
        }
    }

    #[test]
    fn clearing_selection_keeps_the_scene_open_and_allows_reselection() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut page = Places::new();
        page.set_session(session());
        page.pick(Some((0x10, 0)), PickMode::Replace);
        frame(&ctx, &mut page, "");
        let target = page.view.target_xyz();
        let before = page.session.as_ref().unwrap().to_project_json();
        page.pick(None, PickMode::Replace);
        // Clear the previous viewport so this assertion requires another actual render.
        page.view = View3d::new();
        page.view.goto(target, 9.0);
        frame(&ctx, &mut page, "");
        assert!(page.primary().is_none());
        assert!(page.view.viewport().is_some(), "a scene is visible without selection");
        assert_eq!(page.view.target_xyz(), target);
        page.pick(Some((0x10, 1)), PickMode::Replace);
        frame(&ctx, &mut page, "");
        assert_eq!(page.primary(), Some((0x10, 1)));
        assert!(page.view.viewport().is_some());
        assert_eq!(page.session.as_ref().unwrap().to_project_json(), before);
    }

    #[test]
    fn switching_sessions_clears_the_previous_project_export_and_search_identity() {
        let mut page = Places::new();
        page.set_session(session());
        page.project = Some(PathBuf::from("old.sabproject.json"));
        page.staged_dir = Some(PathBuf::from("old_stage"));
        page.report = Some("old static report".into());
        page.goto_cache = Some(("Locator".into(), vec![], 1));
        page.pick(Some((0x10, 1)), PickMode::Replace);
        page.set_session(session());
        assert!(page.project.is_none() && page.staged_dir.is_none() && page.report.is_none());
        assert!(page.goto_cache.is_none() && page.primary().is_none());
        assert!(!page.is_dirty());
    }

    #[test]
    fn cell_project_focus_prefers_the_latest_edited_object_and_falls_back_to_a_position() {
        let mut page = Places::new();
        page.set_session(session());
        page.focus_cell_objects();
        assert_eq!(page.primary(), Some((0x10, 0)));
        page.move_many(&[(0x10, 1, [8.0, 9.0, 10.0])]);
        page.pick(None, PickMode::Replace);
        page.focus_cell_objects();
        assert_eq!(page.primary(), Some((0x10, 1)));
    }

    /// The page draws with and without data, preselects an object, and its actions (the same methods
    /// the buttons call) move, undo, redo and stage through the session.
    #[test]
    fn the_page_renders_and_its_actions_select_move_undo_redo_and_stage() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let dir = std::env::temp_dir().join(format!("sab_places_ui_{}", std::process::id()));
        let out = dir.to_string_lossy().to_string();
        let mut p = Places::new();
        frame(&ctx, &mut p, &out);
        assert_eq!(p.status_line(), "EditNodes not opened");

        p.set_session(session());
        p.preselect(0x10, 0xB);
        frame(&ctx, &mut p, &out);
        assert_eq!((p.sel_container, p.sel_object), (Some(0x10), Some(1)));
        assert_eq!(p.pos_edit, [4.0, 5.0, 6.0], "the inspector fills the fields from the object");

        p.pos_edit = [40.0, 5.0, 6.0];
        p.apply_move();
        frame(&ctx, &mut p, &out);
        assert_eq!(pos(&p), [40.0, 5.0, 6.0]);
        assert_eq!(p.session.as_ref().unwrap().active().len(), 1);

        p.undo();
        frame(&ctx, &mut p, &out);
        assert_eq!(pos(&p), [4.0, 5.0, 6.0]);
        assert_eq!(p.pos_edit, [4.0, 5.0, 6.0], "the fields follow undo");

        p.redo();
        frame(&ctx, &mut p, &out);
        assert_eq!(pos(&p), [40.0, 5.0, 6.0]);

        p.off_edit = [0.5, -1.0, 0.0];
        p.apply_offset();
        assert_eq!(pos(&p), [40.5, 4.0, 6.0], "the offset adds to the stored position");
        p.undo();
        assert_eq!(pos(&p), [40.0, 5.0, 6.0]);
        frame(&ctx, &mut p, &out);

        p.stage(&out);
        let (msg, err) = p.status.clone().unwrap();
        assert!(!err, "{msg}");
        assert!(p.report.as_deref().unwrap_or("").contains("ALL STATIC CHECKS PASS: true"));
        let staged = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        assert!(staged.join("EditNodes.pack").is_file() && staged.join("install.ps1").is_file());
        frame(&ctx, &mut p, &out);

        p.filter = "0x00000020".into();
        frame(&ctx, &mut p, &out);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The viewport's Rotate action: a turn about X rewrites only the basis, is one undo step, and undo restores the bytes.
    #[test]
    fn rotate_many_turns_the_basis_as_one_undo_step() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut node = object(0xB, [4.0, 5.0, 6.0]);
        if let Node::Container { children, .. } = &mut node {
            for (tag, v) in [(tags::X_AXIS, [1.0f32, 0.0, 0.0]), (tags::Y_AXIS, [0.0, 1.0, 0.0]), (tags::Z_AXIS, [0.0, 0.0, 1.0])] {
                children.push(Node::Leaf { tag, data: v.iter().flat_map(|x| x.to_le_bytes()).collect() });
            }
        }
        let base = Pack { entries: vec![Entry { hash: 0x10, root: Root::EditNode { objects: vec![node] } }] };
        let dlc = Pack { entries: vec![Entry { hash: 0x20, root: Root::EditNode { objects: vec![object(0xC, [7.0, 8.0, 9.0])] } }] };
        let mut p = Places::new();
        p.set_session(Session::from_sources(Path::new("G"), EditNodesSources::from_bytes(base.write(), dlc.write(), "LF".into()).unwrap()));
        let y = |p: &Places| p.session.as_ref().unwrap().entry(0x10).unwrap().objects()[0].vec3_param(tags::Y_AXIS).unwrap();
        let before = y(&p);
        let (s, c) = (25f32.to_radians().sin(), 25f32.to_radians().cos());
        p.rotate_many(&ctx, &[(0x10, 0)], [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]], [0.0; 3]);
        assert!(!p.status.clone().unwrap().1, "rotate must not be refused: {:?}", p.status);
        let after = y(&p);
        assert!((after[1] - c).abs() < 1e-5 && (after[2] - s).abs() < 1e-5, "{before:?} -> {after:?}");
        assert_eq!(p.session.as_ref().unwrap().entry(0x10).unwrap().objects()[0].position().unwrap(), [4.0, 5.0, 6.0], "a single-object rotation leaves Position alone");
        assert_eq!(p.session.as_ref().unwrap().active().len(), 2, "the Y and Z basis leaves changed (X is on the axis): one group");
        p.undo();
        assert_eq!(y(&p), before);
    }

    /// GUI-to-game acceptance, authoring half (retail-gated): the Gate A+ chair is edited through the page's own actions
    /// (inspector Apply move + the viewport's Rotate), saved, reopened, staged, and the staged pack is decoded and compared with
    /// values computed here WITHOUT the editor. Writes the frozen expectations to output/m0/gui_accept/. The in-game half
    /// (install, readback, restore) is docs/level_editor/gui_accept_run.ps1.
    #[test]
    #[ignore = "needs the retail install named by .sab-local.toml; run with --ignored"]
    fn gui_accept_author_the_chair_move_and_rotation_and_check_the_export() {
        const CHAIR: u32 = 0x878c_cdfa;
        let game = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
        let retail = Session::open(&game).unwrap();
        let (hits, _) = sab_tools::areas::find_objects(&retail, "878ccdfa", 5);
        let hit = hits.iter().find(|h| h.tag == CHAIR).expect("chair in retail EditNodes");
        let (cont, idx) = hit.obj;
        let o0 = &retail.entry(cont).unwrap().objects()[idx];
        let base_pos = o0.position().unwrap();
        let base_axes = [o0.vec3_param(tags::X_AXIS).unwrap(), o0.vec3_param(tags::Y_AXIS).unwrap(), o0.vec3_param(tags::Z_AXIS).unwrap()];
        drop(retail);

        // intended values, computed independently of the editor
        let dy = 1.0f32;
        let want_pos = [base_pos[0], base_pos[1] + dy, base_pos[2]];
        let (s, c) = (25f32.to_radians().sin(), 25f32.to_radians().cos());
        let rx = |v: [f32; 3]| [v[0], c * v[1] - s * v[2], s * v[1] + c * v[2]];
        let want_axes = [rx(base_axes[0]), rx(base_axes[1]), rx(base_axes[2])];

        let ctx = egui::Context::default();
        theme::install(&ctx);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("gui_accept");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.to_string_lossy().to_string();
        let mut p = Places::new();
        p.set_session(Session::open(&game).unwrap());
        p.preselect(cont, CHAIR);
        frame(&ctx, &mut p, &out);
        assert_eq!((p.sel_container, p.sel_object), (Some(cont), Some(idx)), "the page selected the chair");
        assert_eq!(p.pos_edit, base_pos, "the inspector shows the retail position");
        p.pos_edit = want_pos;
        p.apply_move();
        p.rotate_many(&ctx, &[(cont, idx)], [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]], [0.0; 3]);
        frame(&ctx, &mut p, &out);
        assert!(!p.status.clone().unwrap().1, "{:?}", p.status);

        // save, reopen: the same edits must come back
        let project = dir.join("gui_accept.sabproject.json");
        p.save_project_to(&project);
        let reopened = Session::load(&project).unwrap();
        let o1 = &reopened.entry(cont).unwrap().objects()[idx];
        assert_eq!(o1.position().unwrap(), want_pos, "reopened project keeps the move");
        assert_eq!(reopened.active().len(), p.session.as_ref().unwrap().active().len(), "same journal after reopen");

        // stage from the REOPENED project and decode the exported binary
        let staged = reopened.stage(&dir.join(format!("stage_{}", stamp()))).unwrap();
        assert!(staged.all_pass, "static checks: {}", staged.report);
        let bytes = std::fs::read(staged.out_dir.join("EditNodes.pack")).unwrap();
        let pack = sab_formats::editnodes::Pack::parse(&bytes).unwrap();
        let mut changed = 0usize;
        for e in &pack.entries {
            if let sab_formats::editnodes::Root::EditNode { objects } = &e.root {
                for n in objects {
                    if e.hash == cont && n.tag() == CHAIR {
                        let pos = n.position().unwrap();
                        assert_eq!(pos, want_pos, "exported Position");
                        for (k, tag) in [tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS].into_iter().enumerate() {
                            let a = n.vec3_param(tag).unwrap();
                            assert!((0..3).all(|j| (a[j] - want_axes[k][j]).abs() < 1e-5), "exported axis {k}: {a:?} vs {:?}", want_axes[k]);
                        }
                        changed += 1;
                    }
                }
            }
        }
        assert_eq!(changed, 1, "the chair appears exactly once in the exported container");
        // no neighbour changed: every other object in the exported container equals retail
        let retail = Session::open(&game).unwrap();
        let ex = pack.entries.iter().find(|e| e.hash == cont).expect("container exported");
        if let sab_formats::editnodes::Root::EditNode { objects } = &ex.root {
            let rt = retail.entry(cont).unwrap().objects();
            assert_eq!(objects.len(), rt.len(), "object count unchanged");
            for (i, (a, b)) in objects.iter().zip(rt.iter()).enumerate() {
                if i != idx {
                    assert_eq!(a, b, "neighbour {i} must be byte-identical to retail");
                }
            }
        }
        let want_fwd = rx([-0.98469, 0.0, 0.17429]);
        let j = serde_json::json!({
            "object": { "name": "P_INT_chair2(3)", "tag": format!("{CHAIR:08x}"), "container": format!("{cont:08x}"), "index": idx, "interior": "Belle" },
            "retail_baseline": { "position": base_pos, "axes_xyz": base_axes, "dev_object_state_forward": [-0.98469, 0.0, 0.17429], "dev_object_state_position": [-77.67114, 263.20004, -757.04547] },
            "intended_edit": { "move_dy": dy, "rotate_x_degrees": 25 },
            "expected_after": { "position": want_pos, "axes_xyz": want_axes, "forward_if_forward_is_what_Dev.ObjectState_returns": want_fwd },
            "staged_pack": staged.out_dir.join("EditNodes.pack").to_string_lossy(),
            "staged_pack_sha256": staged.artifact_sha,
            "project": project.to_string_lossy(),
        });
        std::fs::write(dir.join("expected.json"), serde_json::to_string_pretty(&j).unwrap()).unwrap();
    }

    /// Duplicate, offline half (retail-gated): the chair is duplicated through the page's Duplicate action, saved, reopened,
    /// staged, and the exported container is decoded: the original is byte-identical to retail, exactly one new object exists
    /// with a fresh tag, the same class and the position one snap step (1 m, X) away; no other object changed. Not loaded in game.
    #[test]
    #[ignore = "needs the retail install named by .sab-local.toml; run with --ignored"]
    fn duplicate_author_the_chair_copy_and_check_the_export() {
        const CHAIR: u32 = 0x878c_cdfa;
        let game = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
        let retail = Session::open(&game).unwrap();
        let (hits, _) = sab_tools::areas::find_objects(&retail, "878ccdfa", 5);
        let (cont, idx) = hits.iter().find(|h| h.tag == CHAIR).expect("chair").obj;
        let rt: Vec<Node> = retail.entry(cont).unwrap().objects().to_vec();
        let base_pos = rt[idx].position().unwrap();
        let all_tags: std::collections::HashSet<u32> = retail.containers().iter().filter_map(|c| retail.entry(c.hash)).flat_map(|e| e.objects().iter().map(|o| o.tag())).collect();
        drop(retail);

        let ctx = egui::Context::default();
        theme::install(&ctx);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("gui_accept");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = Places::new();
        p.set_session(Session::open(&game).unwrap());
        p.preselect(cont, CHAIR);
        frame(&ctx, &mut p, &dir.to_string_lossy());
        assert_eq!(p.selected(), vec![(cont, idx)]);
        p.duplicate(&ctx);
        assert!(!p.status.clone().unwrap().1, "{:?}", p.status);

        let project = dir.join("gui_accept_duplicate.sabproject.json");
        p.save_project_to(&project);
        let reopened = Session::load(&project).unwrap();
        let staged = reopened.stage(&dir.join(format!("stage_dup_{}", stamp()))).unwrap();
        assert!(staged.all_pass, "static checks: {}", staged.report);
        let pack = sab_formats::editnodes::Pack::parse(&std::fs::read(staged.out_dir.join("EditNodes.pack")).unwrap()).unwrap();
        let ex = pack.entries.iter().find(|e| e.hash == cont).expect("container exported");
        let sab_formats::editnodes::Root::EditNode { objects } = &ex.root else { panic!("not an EditNode entry") };
        assert_eq!(objects.len(), rt.len() + 1, "exactly one object added");
        let orig = objects.iter().find(|o| o.tag() == CHAIR).expect("original still present");
        assert_eq!(orig, &rt[idx], "the original chair is byte-identical to retail");
        let new: Vec<&Node> = objects.iter().filter(|o| !rt.iter().any(|r| r.tag() == o.tag())).collect();
        assert_eq!(new.len(), 1, "exactly one object with a new tag");
        let n = new[0];
        assert!(!all_tags.contains(&n.tag()), "the minted tag {:08x} collides with no retail EditNodes object", n.tag());
        assert_eq!(n.class_name(), rt[idx].class_name(), "same class");
        let np = n.position().unwrap();
        assert_eq!(np, [base_pos[0] + 1.0, base_pos[1], base_pos[2]], "one snap step along X");
        for o in rt.iter().filter(|r| r.tag() != CHAIR) {
            assert!(objects.iter().any(|x| x == o), "neighbour {:08x} unchanged", o.tag());
        }
        let j = serde_json::json!({ "original_tag": format!("{CHAIR:08x}"), "duplicate_tag": format!("{:08x}", n.tag()), "duplicate_position": np, "container": format!("{cont:08x}"),
            "staged_pack": staged.out_dir.join("EditNodes.pack").to_string_lossy(), "staged_pack_sha256": staged.artifact_sha, "loaded_in_game": false });
        std::fs::write(dir.join("expected_duplicate.json"), serde_json::to_string_pretty(&j).unwrap()).unwrap();
    }

    /// Delete, offline half (retail-gated): the chair is deleted through the page's Delete action, saved, reopened, staged, and the exported
    /// container decoded: exactly one object fewer, the chair gone, every other object byte-identical to retail and in the same order.
    #[test]
    #[ignore = "needs the retail install named by .sab-local.toml; run with --ignored"]
    fn delete_author_the_chair_removal_and_check_the_export() {
        const CHAIR: u32 = 0x878c_cdfa;
        let game = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
        let retail = Session::open(&game).unwrap();
        let (hits, _) = sab_tools::areas::find_objects(&retail, "878ccdfa", 5);
        let (cont, idx) = hits.iter().find(|h| h.tag == CHAIR).expect("chair").obj;
        let rt: Vec<Node> = retail.entry(cont).unwrap().objects().to_vec();
        drop(retail);

        let ctx = egui::Context::default();
        theme::install(&ctx);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("gui_accept");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = Places::new();
        p.set_session(Session::open(&game).unwrap());
        p.preselect(cont, CHAIR);
        frame(&ctx, &mut p, &dir.to_string_lossy());
        assert_eq!(p.selected(), vec![(cont, idx)]);
        p.delete(&ctx);
        assert!(!p.status.clone().unwrap().1, "{:?}", p.status);

        let project = dir.join("gui_accept_delete.sabproject.json");
        p.save_project_to(&project);
        let reopened = Session::load(&project).unwrap();
        let staged = reopened.stage(&dir.join(format!("stage_del_{}", stamp()))).unwrap();
        assert!(staged.all_pass, "static checks: {}", staged.report);
        let pack = sab_formats::editnodes::Pack::parse(&std::fs::read(staged.out_dir.join("EditNodes.pack")).unwrap()).unwrap();
        let ex = pack.entries.iter().find(|e| e.hash == cont).expect("container exported");
        let sab_formats::editnodes::Root::EditNode { objects } = &ex.root else { panic!("not an EditNode entry") };
        assert_eq!(objects.len(), rt.len() - 1, "exactly one object fewer");
        assert!(!objects.iter().any(|o| o.tag() == CHAIR), "the chair is gone");
        let expect: Vec<&Node> = rt.iter().filter(|o| o.tag() != CHAIR).collect();
        assert!(objects.iter().zip(expect.iter()).all(|(a, b)| a == *b), "every other object is byte-identical to retail, in the same order");
        let neighbours: Vec<String> = [idx - 2, idx - 1, idx + 1, idx + 2].iter().filter_map(|i| rt.get(*i)).map(|o| format!("{:08x}", o.tag())).collect();
        let j = serde_json::json!({ "deleted_tag": format!("{CHAIR:08x}"), "neighbour_tags": neighbours, "container": format!("{cont:08x}"), "objects_before": rt.len(), "objects_after": objects.len(),
            "staged_pack": staged.out_dir.join("EditNodes.pack").to_string_lossy(), "staged_pack_sha256": staged.artifact_sha, "loaded_in_game": false });
        std::fs::write(dir.join("expected_delete.json"), serde_json::to_string_pretty(&j).unwrap()).unwrap();
    }

    /// Terrain-cell mode is a feature flag: off, opening a cell is refused with the reason and nothing else changes; the EditNodes session is untouched.
    #[test]
    fn terrain_cell_mode_is_off_by_default_and_refuses_without_the_flag() {
        let mut p = Places::new();
        p.cells_enabled = false;
        p.set_session(session());
        p.open_cell("G", 1_700_865);
        let (msg, err) = p.status.clone().unwrap();
        assert!(err && msg.contains("SAB_WORKSHOP_TERRAIN_CELLS"), "{msg}");
        assert!(p.cell_edit.is_none() && p.session.is_some(), "the current EditNodes session is untouched");
        assert!(p.session.as_ref().unwrap().entry(0x10).is_some());
    }

    /// Terrain-cell authoring from the page (retail-gated): the page's own actions (open cell, preselect, inspector Apply move, Stage mod) move the E2b slab 4 m and the staged
    /// pack is BYTE-IDENTICAL to the pack seen working in game (output/m0/e2b). Also: undo works, the project saves and reopens in cell mode, a class edit is refused at export.
    #[test]
    #[ignore = "needs the retail install named by .sab-local.toml; run with --ignored (takes ~1-2 minutes)"]
    fn terrain_cell_authoring_from_the_page_matches_the_pack_seen_in_game() {
        const CELL: u32 = 1_700_865;
        const SLAB: u32 = 0x80a9_bb53;
        let game = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
        let ctx = egui::Context::default();
        theme::install(&ctx);
        // kept on disk (not deleted) so the live game test can install exactly this Workshop-authored export
        let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("cell_ws");
        let _ = std::fs::remove_dir_all(&out_dir);
        let out = out_dir.to_string_lossy().to_string();
        let mut p = Places::new();
        p.cells_enabled = true;
        p.open_cell(&game.to_string_lossy(), CELL);
        assert!(!p.status.clone().unwrap().1, "{:?}", p.status);
        let (li, idx) = p.cell_edit.as_ref().unwrap().find(SLAB).expect("slab in its cell");
        let cont = p.cell_edit.as_ref().unwrap().containers[li];
        p.preselect(cont, SLAB);
        frame(&ctx, &mut p, &out);
        assert_eq!((p.sel_container, p.sel_object), (Some(cont), Some(idx)), "the page selected the slab");
        let old = p.pos_edit;
        assert!((old[1] - 54.4976).abs() < 1e-3, "inspector shows the retail Position: {old:?}");
        p.pos_edit = [old[0], old[1] + 4.0, old[2]];
        p.apply_move();
        frame(&ctx, &mut p, &out);
        assert!(!p.status.clone().unwrap().1, "{:?}", p.status);
        // undo and redo through the same page actions
        p.undo();
        assert_eq!(p.session.as_ref().unwrap().entry(cont).unwrap().objects()[idx].position().unwrap(), old);
        p.redo();
        // the project saves and reopens in this mode (terrain-cell project: cell id + journal)
        std::fs::create_dir_all(&out_dir).unwrap();
        let proj = out_dir.join("x.sabproject.json");
        p.save_project_to(&proj);
        assert!(!p.status.clone().unwrap().1 && proj.exists(), "{:?}", p.status);
        let n_active = p.session.as_ref().unwrap().active().len();
        let mut q = Places::new();
        q.cells_enabled = true;
        q.open_project_from(&proj);
        assert!(!q.status.clone().unwrap().1 && q.cell_edit.as_ref().map(|c| c.cell) == Some(CELL), "{:?}", q.status);
        assert_eq!(q.session.as_ref().unwrap().active().len(), n_active, "the reopened session has the same edits");
        assert_eq!(q.session.as_ref().unwrap().entry(cont).unwrap().objects()[idx].position().unwrap(), p.session.as_ref().unwrap().entry(cont).unwrap().objects()[idx].position().unwrap());
        let mut off = Places::new();
        off.cells_enabled = false;
        off.open_project_from(&proj);
        assert!(off.status.clone().unwrap().1 && off.session.is_none(), "without the flag a cell project is refused: {:?}", off.status);
        // stage
        std::fs::create_dir_all(&out_dir).unwrap();
        p.game_dir = game.to_string_lossy().to_string();
        p.stage(&out);
        let (msg, err) = p.status.clone().unwrap();
        assert!(!err && msg.contains("patchmega1.megapack"), "{msg}");
        assert!(p.report.as_deref().unwrap_or("").contains("ALL STATIC CHECKS PASS: true"));
        let staged = std::fs::read_dir(&out_dir).unwrap().filter_map(|e| e.ok()).find(|e| e.file_name().to_string_lossy().starts_with("cell_stage_")).unwrap().path();
        let got = std::fs::read(staged.join("patchmega1.megapack")).unwrap();
        let e2b = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("e2b").join("patchmega1.megapack");
        if e2b.is_file() {
            assert_eq!(sab_tools::stage::sha256(&got), sab_tools::stage::sha256(&std::fs::read(&e2b).unwrap()), "the page-authored pack equals the pack seen in game");
        }
        assert!(staged.join("install.ps1").is_file() && staged.join("restore.ps1").is_file());
        // an edit the cell export does not support (a class change) is refused at export, with nothing written
        let (c2, i2) = (cont, idx);
        p.session.as_mut().unwrap().set_class_name(c2, i2, "P_INT_RoundTable_Big").unwrap();
        p.stage(&out);
        let (m, e) = p.status.clone().unwrap();
        assert!(e && m.contains("duplicates and deletions only"), "{m}");
    }

    /// Terrain-cell ROTATION authored from the page (retail-gated): open the cell, select the slab, rotate it 45 degrees about X through the viewport's Rotate handler (`rotate_many`),
    /// undo/redo, Stage mod. The export is kept in output/m0/cell_ws_rot for the live A/B; the decode check is `tools/sab_tools/tests/cell_session_retail.rs`.
    #[test]
    #[ignore = "needs the retail install named by .sab-local.toml; run with --ignored (takes ~1-2 minutes)"]
    fn terrain_cell_rotation_authored_from_the_page_stages_cleanly() {
        const CELL: u32 = 1_700_865;
        const SLAB: u32 = 0x80a9_bb53;
        let game = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("cell_ws_rot");
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();
        let out = out_dir.to_string_lossy().to_string();
        let mut p = Places::new();
        p.cells_enabled = true;
        p.open_cell(&game.to_string_lossy(), CELL);
        assert!(!p.status.clone().unwrap().1, "{:?}", p.status);
        let (li, idx) = p.cell_edit.as_ref().unwrap().find(SLAB).expect("slab in its cell");
        let cont = p.cell_edit.as_ref().unwrap().containers[li];
        p.preselect(cont, SLAB);
        frame(&ctx, &mut p, &out);
        let before = p.session.as_ref().unwrap().entry(cont).unwrap().objects()[idx].clone();
        let (sn, cs) = (45f32.to_radians().sin(), 45f32.to_radians().cos());
        p.rotate_many(&ctx, &[(cont, idx)], [[1.0, 0.0, 0.0], [0.0, cs, -sn], [0.0, sn, cs]], [0.0; 3]);
        assert!(!p.status.clone().unwrap().1, "{:?}", p.status);
        assert_ne!(p.session.as_ref().unwrap().entry(cont).unwrap().objects()[idx], before, "the rotation changed the object");
        p.undo();
        assert_eq!(p.session.as_ref().unwrap().entry(cont).unwrap().objects()[idx], before, "undo restores the exact bytes");
        p.redo();
        p.game_dir = game.to_string_lossy().to_string();
        p.stage(&out);
        let (msg, err) = p.status.clone().unwrap();
        assert!(!err && msg.contains("patchmega1.megapack"), "{msg}");
        assert!(p.report.as_deref().unwrap_or("").contains("ALL STATIC CHECKS PASS: true"));
        assert!(p.report.as_deref().unwrap().matches("leaf @").count() >= 1);
    }

    /// Terrain-cell DELETE and DUPLICATE authored from the page (retail-gated). Each export is kept on disk for the live A/B:
    /// output/m0/cell_ws_del (slab deleted) and output/m0/cell_ws_dup (slab duplicated, the copy moved by (-2, 0, -2) so it is visible beside Sean).
    #[test]
    #[ignore = "needs the retail install named by .sab-local.toml; run with --ignored (takes ~2-4 minutes)"]
    fn terrain_cell_delete_and_duplicate_authored_from_the_page_stage_cleanly() {
        const CELL: u32 = 1_700_865;
        const SLAB: u32 = 0x80a9_bb53;
        let game = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0");
        for mode in ["del", "dup"] {
            let out_dir = root.join(format!("cell_ws_{mode}"));
            let _ = std::fs::remove_dir_all(&out_dir);
            std::fs::create_dir_all(&out_dir).unwrap();
            let out = out_dir.to_string_lossy().to_string();
            let mut p = Places::new();
            p.cells_enabled = true;
            p.open_cell(&game.to_string_lossy(), CELL);
            assert!(!p.status.clone().unwrap().1, "{:?}", p.status);
            let (li, idx) = p.cell_edit.as_ref().unwrap().find(SLAB).expect("slab in its cell");
            let cont = p.cell_edit.as_ref().unwrap().containers[li];
            let n0 = p.session.as_ref().unwrap().entry(cont).unwrap().objects().len();
            p.preselect(cont, SLAB);
            frame(&ctx, &mut p, &out);
            let orig = p.pos_edit;
            if mode == "del" {
                p.delete(&ctx);
                assert_eq!(p.session.as_ref().unwrap().entry(cont).unwrap().objects().len(), n0 - 1);
                p.undo();
                assert_eq!(p.session.as_ref().unwrap().entry(cont).unwrap().objects().len(), n0, "undo restores the object");
                p.redo();
            } else {
                p.duplicate(&ctx);
                assert_eq!(p.session.as_ref().unwrap().entry(cont).unwrap().objects().len(), n0 + 1);
                assert_eq!(p.selected().len(), 1, "the copy is selected");
                frame(&ctx, &mut p, &out);
                p.pos_edit = [orig[0] - 2.0, orig[1], orig[2] - 2.0];
                p.apply_move();
                let (sc, si) = (p.sel_container.unwrap(), p.sel_object.unwrap());
                assert_eq!(p.session.as_ref().unwrap().entry(sc).unwrap().objects()[si].position().unwrap(), [orig[0] - 2.0, orig[1], orig[2] - 2.0]);
                assert_ne!(p.session.as_ref().unwrap().entry(sc).unwrap().objects()[si].tag(), SLAB, "the copy has its own tag");
            }
            p.game_dir = game.to_string_lossy().to_string();
            p.stage(&out);
            let (msg, err) = p.status.clone().unwrap();
            assert!(!err && msg.contains("patchmega1.megapack"), "{mode}: {msg}");
            let rep = p.report.clone().unwrap_or_default();
            assert!(rep.contains("ALL STATIC CHECKS PASS: true") && rep.contains("REBUILD"), "{mode}: {rep}");
        }
    }

    /// One egui frame of the page with an explicit game folder (what the app passes each frame).
    fn frame_with(ctx: &egui::Context, p: &mut Places, gd: &str, out: &str) {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0))), ..Default::default() };
        let _ = ctx.run(input, |c| p.show(c, gd, out));
    }

    /// Deliver a real pointer click through egui at the centre of a recorded toolbar button: move, press, release (the click registers on release).
    fn click_button(ctx: &egui::Context, p: &mut Places, out: &str, name: &'static str) {
        let r = *p.test_rects.get(name).unwrap_or_else(|| panic!("button '{name}' was not drawn (is it hidden in this mode?)"));
        let pos = r.center();
        let mods = egui::Modifiers::default();
        let step = |p: &mut Places, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0))), events, ..Default::default() };
            let _ = ctx.run(input, |ctx| p.show(ctx, &p.game_dir.clone(), out));
        };
        step(p, vec![egui::Event::PointerMoved(pos)]);
        step(p, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: mods }]);
        step(p, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: mods }]);
        step(p, vec![]);
    }

    /// The real toolbar buttons, for a terrain-cell project (retail-gated): session A opens cell 1700865, moves the slab, and the "Save project..." BUTTON is clicked; a brand-new Places
    /// (a relaunched Workshop: it auto-opens the ordinary EditNodes session first) has the "Open project..." BUTTON clicked; the previous edit is present; a further edit is made through
    /// the page; the "Stage mod" BUTTON is clicked and the exported pack holds both edits. File dialogs are answered by a test-only seam; pointer events are egui-level, not OS input.
    #[test]
    #[ignore = "needs the retail install named by .sab-local.toml; run with --ignored (takes several minutes)"]
    fn the_real_save_open_and_stage_buttons_work_for_a_terrain_cell_project() {
        const CELL: u32 = 1_700_865;
        const SLAB: u32 = 0x80a9_bb53;
        const PANEL: u32 = 0x1c96_23d1;
        let game = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
        let gd = game.to_string_lossy().to_string();
        let dir = std::env::temp_dir().join(format!("sab_cellgui_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.to_string_lossy().to_string();
        let proj = dir.join("gui.sabproject.json");
        let ctx = egui::Context::default();
        theme::install(&ctx);

        // ---- session A: edit, then the Save button
        let mut a = Places::new();
        a.cells_enabled = true;
        a.open_cell(&gd, CELL);
        assert!(!a.status.clone().unwrap().1, "{:?}", a.status);
        let (li, idx) = a.cell_edit.as_ref().unwrap().find(SLAB).unwrap();
        let cont = a.cell_edit.as_ref().unwrap().containers[li];
        a.preselect(cont, SLAB);
        frame_with(&ctx, &mut a, &gd, &out);
        let p0 = a.pos_edit;
        a.pos_edit = [p0[0], p0[1] + 4.0, p0[2]];
        a.apply_move();
        frame_with(&ctx, &mut a, &gd, &out);
        a.test_dialog = Some(proj.clone());
        click_button(&ctx, &mut a, &out, "save");
        assert!(proj.exists(), "the Save button wrote the project: {:?}", a.status);
        assert_eq!(sab_tools::cellsession::cell_project_id(&proj), Some(CELL));
        assert!(!a.status.clone().unwrap().1 && a.status.clone().unwrap().0.contains("terrain-cell project"), "{:?}", a.status);
        drop(a);

        // ---- a relaunched Workshop: fresh Places, the ordinary session opens first, then the Open button
        let mut b = Places::new();
        b.cells_enabled = true;
        frame_with(&ctx, &mut b, &gd, &out);
        assert!(b.cell_edit.is_none() && b.session.is_some(), "a fresh launch starts on the ordinary EditNodes session");
        b.test_dialog = Some(proj.clone());
        click_button(&ctx, &mut b, &out, "open");
        assert!(b.cell_edit.as_ref().map(|c| c.cell) == Some(CELL), "the Open button reopened the cell project: {:?}", b.status);
        let (li_b, _) = b.cell_edit.as_ref().unwrap().find(SLAB).unwrap();
        let cont_b = b.cell_edit.as_ref().unwrap().containers[li_b];
        let slab_b = b.session.as_ref().unwrap().entry(cont_b).unwrap().objects().iter().find(|o| o.tag() == SLAB).unwrap().position().unwrap();
        assert_eq!(slab_b, [p0[0], p0[1] + 4.0, p0[2]], "the previous edit (+4 m on the slab) is present after the reopen");
        assert_eq!(b.session.as_ref().unwrap().active().len(), 1);

        // ---- a further edit through the page, then the Stage button
        b.preselect(cont_b, PANEL);
        frame_with(&ctx, &mut b, &gd, &out);
        let pp = b.pos_edit;
        b.pos_edit = [pp[0] + 1.0, pp[1], pp[2]];
        b.apply_move();
        frame_with(&ctx, &mut b, &gd, &out);
        assert_eq!(b.session.as_ref().unwrap().active().len(), 2, "a further edit is possible after the reopen");
        click_button(&ctx, &mut b, &out, "stage");
        let (msg, err) = b.status.clone().unwrap();
        assert!(!err && msg.contains("patchmega1.megapack"), "the Stage button exported: {msg}");
        let staged = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).find(|e| e.file_name().to_string_lossy().starts_with("cell_stage_")).expect("a cell_stage folder").path();
        let mp = std::fs::read(staged.join("patchmega1.megapack")).unwrap();
        let pk = sab_formats::megapack::parse(&mp).unwrap();
        let sub = sab_formats::megapack::entry_slice(&mp, &pk.entries[0]).unwrap();
        let cell = sab_formats::cellplace::read(sub).unwrap().unwrap();
        let l = &cell.lists[li_b];
        let nodes = sab_formats::editnodes::parse_entry(&sub[l.loc.off..l.loc.off + l.loc.comp]).unwrap();
        let get = |t: u32| nodes.iter().find(|n| n.tag() == t).unwrap();
        assert_eq!(get(SLAB).position().unwrap(), [p0[0], p0[1] + 4.0, p0[2]], "the exported pack holds the edit made before the save");
        assert_eq!(get(PANEL).position().unwrap(), [pp[0] + 1.0, pp[1], pp[2]], "and the edit made after the reopen");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Terrain read-outs on the page: nothing before the index is loaded; once it is ready, hovering inside the 3D view produces the
    /// "terrain under the cursor" line (a hit, or an explicit none), and the selected object's ground report is computable. Read-only.
    #[test]
    fn terrain_readouts_appear_once_the_index_is_ready() {
        use sab_terrain::{geometry::TerrainMesh, retail::ChunkSource};
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut p = Places::new();
        p.set_session(session());
        p.preselect(0x10, 0xB);
        frame(&ctx, &mut p, "");
        let rect = p.view.last_rect().expect("the view was drawn");
        let hover = |p: &mut Places| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0))), events: vec![egui::Event::PointerMoved(rect.center())], ..Default::default() };
            let _ = ctx.run(input, |c| p.show(c, "", ""));
        };
        hover(&mut p);
        assert!(p.terrain_hover.is_none(), "no terrain read-out before the index is loaded");

        // a 2 km square of terrain at y = 0 under the objects (two triangles), installed as a ready index
        let mesh = TerrainMesh {
            name: "synthetic".into(),
            streams: vec![],
            vertices: vec![[-1000.0, 0.0, -1000.0], [1000.0, 0.0, -1000.0], [1000.0, 0.0, 1000.0], [-1000.0, 0.0, 1000.0]],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
            bounds: [[-1000.0, 0.0, -1000.0], [1000.0, 0.0, 1000.0]],
        };
        let mut b = sab_terrain_query::Builder::default();
        b.add(&ChunkSource { pack: "synthetic".into(), cell: 1, crc: 1, offset: 0 }, &mesh).unwrap();
        p.terrain.set_ready(b.finish());
        hover(&mut p);
        let t = p.terrain_hover.clone().expect("hovering inside the view gives a terrain read-out");
        assert!(t.starts_with("terrain under the cursor:"), "{t}");
        // the selected object (4, 5, 6) is 5 m above this terrain
        let r = crate::place_terrain::ground_under(p.terrain.index().unwrap(), [4.0, 5.0, 6.0]).unwrap();
        assert_eq!(crate::place_terrain::describe_ground(&r, [4.0, 5.0, 6.0]), "terrain: 5.00 m above the terrain (surface 0.000 m; 1 surface(s) here)");
        // reading terrain never edits the session
        assert!(p.session.as_ref().unwrap().active().is_empty());
    }

    /// Terrain is on by default; a ready index hands real triangles to the 3D view; a Hill raises them
    /// (serial changes); Reset restores the base; switching off removes it; and none of it touches the edit session. PREVIEW ONLY.
    #[test]
    fn the_terrain_preview_shows_sculpts_resets_and_never_touches_the_session() {
        use sab_terrain::{geometry::TerrainMesh, retail::ChunkSource};
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut p = Places::new();
        assert!(p.terrain_on, "outdoor terrain is visible by default");
        p.terrain_on = false;
        p.set_session(session());
        p.preselect(0x10, 0xA);
        frame(&ctx, &mut p, "");
        assert!(p.view.terrain_state().is_none(), "no terrain before it is loaded and switched on");

        // a 400 m flat terrain at y = 5 under the test objects (around x 100, z 200)
        let mesh = TerrainMesh {
            name: "synthetic".into(),
            streams: vec![],
            vertices: vec![[-100.0, 5.0, 0.0], [300.0, 5.0, 0.0], [300.0, 5.0, 400.0], [-100.0, 5.0, 400.0]],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
            bounds: [[-100.0, 5.0, 0.0], [300.0, 5.0, 400.0]],
        };
        let mut b = sab_terrain_query::Builder::default();
        b.add(&ChunkSource { pack: "synthetic".into(), cell: 1, crc: 1, offset: 0 }, &mesh).unwrap();
        p.terrain.set_ready(b.finish());
        frame(&ctx, &mut p, "");
        assert!(p.view.terrain_state().is_none(), "still off until switched on");

        p.terrain_on = true;
        frame(&ctx, &mut p, "");
        let (s0, tris0) = p.view.terrain_state().expect("terrain triangles reach the view once switched on");
        assert!(tris0 > 1000, "a 320 m patch at 4 m is thousands of triangles, got {tris0}");

        let before = p.session.as_ref().unwrap().to_project_json();
        p.terrain_dab(crate::place_terrain::Brush::Raise);
        frame(&ctx, &mut p, "");
        let (s1, tris1) = p.view.terrain_state().unwrap();
        assert_ne!(s0, s1, "the geometry key changes when the terrain is sculpted");
        assert_eq!(tris0, tris1, "same grid, different heights");
        assert!(p.terrain_patch.as_ref().unwrap().has_edits());
        assert!(p.status.as_ref().unwrap().0.contains("preview only"), "{:?}", p.status);

        p.terrain_demo();
        frame(&ctx, &mut p, "");
        let (s2, _) = p.view.terrain_state().unwrap();
        assert_ne!(s1, s2);

        p.terrain_patch.as_mut().unwrap().reset();
        frame(&ctx, &mut p, "");
        assert!(!p.terrain_patch.as_ref().unwrap().has_edits(), "reset restores the sampled terrain");

        p.terrain_on = false;
        frame(&ctx, &mut p, "");
        assert!(p.view.terrain_state().is_none(), "switching off removes the terrain from the view");
        assert_eq!(p.session.as_ref().unwrap().to_project_json(), before, "terrain preview never touches the edit session");
    }

    fn tags_of(p: &Places, c: u32) -> Vec<u32> {
        p.session.as_ref().unwrap().entry(c).unwrap().objects().iter().map(|o| o.tag()).collect()
    }

    /// Deleting an earlier object shifts later indices; undo and redo must leave the selection on the
    /// objects it named, never on whatever now sits at the old index.
    #[test]
    fn selection_keeps_addressing_its_object_across_delete_undo_redo() {
        let mut p = Places::new();
        p.set_session(session());
        let tag_at = |p: &Places, r: ObjRef| p.session.as_ref().unwrap().entry(r.0).unwrap().objects().get(r.1).map(|o| o.tag());
        let ctx = egui::Context::default();

        // Delete A (index 0), then select B, which has moved to index 0.
        p.pick(Some((0x10, 0)), PickMode::Replace);
        p.delete(&ctx);
        p.pick(Some((0x10, 0)), PickMode::Replace);
        assert_eq!(tag_at(&p, (0x10, 0)), Some(0xB));

        // Undo restores A at 0 and selects it (the step's object); B is now at index 1.
        p.undo();
        assert_eq!(p.selected(), vec![(0x10, 0)]);
        assert_eq!(tag_at(&p, (0x10, 0)), Some(0xA));
        assert_eq!(tag_at(&p, (0x10, 1)), Some(0xB));

        // Redo deletes A again: the selection must not silently become B.
        p.redo();
        assert!(p.selected().is_empty(), "A is gone and B was not selected, got {:?}", p.selected());

        // With B and C selected, a delete of A (index 0) then undo then redo keeps both addressed.
        p.pick(Some((0x10, 0)), PickMode::Replace);
        p.pick(Some((0x20, 0)), PickMode::Add);
        let before: Vec<u32> = p.selected().iter().filter_map(|r| tag_at(&p, *r)).collect();
        assert_eq!(before, vec![0xC, 0xB]);
        p.undo();
        p.redo();
        let after: Vec<u32> = p.selected().iter().filter_map(|r| tag_at(&p, *r)).collect();
        assert!(after.iter().all(|t| *t == 0xB || *t == 0xC || *t == 0xA), "{after:?}");
    }

    /// The 3D view is the main surface: on a laptop-sized window (911×512 points, a 1366×768 screen at
    /// 150 % scaling) it still gets a usable rectangle, and on a large window most of the width.
    #[test]
    fn the_viewport_stays_usable_on_small_windows() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        for (w, h, min_w, min_h) in [(911.0, 512.0, 380.0, 150.0), (1280.0, 720.0, 560.0, 330.0), (1600.0, 1000.0, 760.0, 600.0)] {
            let mut p = Places::new();
            p.set_session(session());
            p.pick(Some((0x10, 1)), PickMode::Replace);
            // The longest realistic status and project path: they must not push the view out.
            p.project = Some(PathBuf::from(r"C:\Users\SomeoneWithALongName\Documents\SAB Workshop\placements.sabproj.json"));
            p.status = Some((
                r"opened LaVillette entrance (exterior): exterior terrain is not drawn · 115 objects nearby. could not save the project: refusing to write inside the game folder: C:\Saboteur\The Saboteur\AnimText\work.sabproj.json. Choose another folder, for example one in Documents.".into(),
                true,
            ));
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h))), ..Default::default() };
            for _ in 0..3 {
                let _ = ctx.run(input.clone(), |ctx| p.show(ctx, "", ""));
            }
            let r = p.view.viewport().unwrap_or_else(|| panic!("no viewport at {w}x{h}"));
            let visible = r.intersect(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h)));
            assert!(visible.width() >= min_w && visible.height() >= min_h, "{w}x{h}: visible viewport only {:?} (rect {:?})", visible.size(), r);
        }
    }

    /// Opening areas, selecting and focusing objects, and drawing the room only read: the journal, the
    /// revision and the dirty flag stay as they were, and nothing is stageable.
    #[test]
    fn browsing_areas_never_enters_the_edit_journal() {
        let mut p = Places::new();
        p.set_session(session());
        let area = Area {
            name: "Test".into(),
            kind: Kind::Interior,
            script: String::new(),
            anchor: Some([4.0, 5.0, 6.0]),
            anchor_note: String::new(),
            anchor_obj: Some((0x10, 1)),
            primary: None,
            primary_note: String::new(),
            map_objects: Vec::new(),
            objects: Vec::new(),
        };
        p.areas = Some(Ok(std::sync::Arc::new(vec![area])));
        let rev = p.session.as_ref().unwrap().revision();
        p.open_area(0);
        p.pick(Some((0x10, 0)), PickMode::Replace);
        p.view.request_focus();
        let s = p.session.as_ref().unwrap();
        assert_eq!((s.revision(), s.journal().0.len(), s.active().len()), (rev, 0, 0));
        assert!(!p.is_dirty());
        assert_eq!(p.selected(), vec![(0x10, 0)]);
    }

    /// Saving a project clears the dirty flag; any later edit, undo or redo sets it again.
    #[test]
    fn unsaved_edits_are_tracked_until_the_project_is_saved() {
        let mut p = Places::new();
        p.set_session(session());
        assert!(!p.is_dirty());
        p.pick(Some((0x10, 1)), PickMode::Replace);
        p.pos_edit = [40.0, 5.0, 6.0];
        p.apply_move();
        assert!(p.is_dirty());
        let file = std::env::temp_dir().join(format!("sab_dirty_{}.json", std::process::id()));
        p.save_project_to(&file);
        assert!(!p.is_dirty(), "{:?}", p.status);
        p.undo();
        assert!(p.is_dirty(), "undo changes the work after the save");
        p.redo();
        assert!(p.is_dirty(), "conservative: redo does not claim to match the file");
        p.save_project_to(&file);
        assert!(!p.is_dirty());
        std::fs::remove_file(&file).ok();
        for k in 1..=5 {
            std::fs::remove_file(format!("{}.bak{k}", file.display())).ok();
        }
    }

    /// Copy/paste mints tags, cut/paste keeps them, duplicate and delete are single undo steps, undo
    /// restores the step's selection, and a prefab round-trips.
    #[test]
    fn clipboard_duplicate_delete_and_prefab_actions() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut p = Places::new();
        p.set_session(session());
        frame(&ctx, &mut p, "");

        p.pick(Some((0x10, 0)), PickMode::Replace);
        p.copy(&ctx);
        p.paste(&ctx, true);
        let t = tags_of(&p, 0x10);
        assert_eq!(t.len(), 3);
        assert!(t[2] != 0xA && t[2] != 0xB, "a pasted copy gets a new tag");
        assert_eq!(p.selected(), vec![(0x10, 2)], "the pasted object is selected");

        p.duplicate(&ctx);
        assert_eq!(tags_of(&p, 0x10).len(), 4);
        let dup_pos = p.session.as_ref().unwrap().entry(0x10).unwrap().objects()[3].position().unwrap();
        assert_eq!(dup_pos, [2.0, 2.0, 3.0], "duplicate moves one snap step (1 m) along X");

        p.pick(Some((0x10, 1)), PickMode::Replace);
        p.cut(&ctx);
        assert!(!tags_of(&p, 0x10).contains(&0xB));
        p.paste(&ctx, true);
        assert!(tags_of(&p, 0x10).contains(&0xB), "a cut's paste keeps the original tag");

        p.pick(Some((0x10, 0)), PickMode::Replace);
        p.delete(&ctx);
        assert!(!tags_of(&p, 0x10).contains(&0xA) && p.selected().is_empty());
        assert_eq!(p.undo().as_deref(), Some("Delete"));
        assert!(tags_of(&p, 0x10).contains(&0xA), "undo restores the deleted object");
        assert_eq!(p.selected(), vec![(0x10, 0)], "and selects it again");

        let dir = std::env::temp_dir().join(format!("sab_prefab_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pf = dir.join("x.sabprefab.json");
        p.pick_many(vec![(0x10, 0), (0x20, 0)], PickMode::Replace);
        assert_eq!(p.export_prefab_to(&pf).unwrap(), 2);
        let before = (tags_of(&p, 0x10).len(), tags_of(&p, 0x20).len());
        p.import_prefab_from(&ctx, &pf).unwrap();
        assert_eq!((tags_of(&p, 0x10).len(), tags_of(&p, 0x20).len()), (before.0 + 1, before.1 + 1), "one object back into each source container");
        std::fs::remove_dir_all(&dir).ok();

        // A LuaParam edit through the same session call the grid uses.
        let s = p.session.as_mut().unwrap();
        s.set_lua_param(0x10, 0, 0, LuaValue::Bool(0)).unwrap();
        assert_eq!(s.group_name(*s.groups().last().unwrap()), "Set bFlag");
        frame(&ctx, &mut p, "");
        assert!(p.session.as_ref().unwrap().validate().iter().any(|i| i.severity == Severity::Note), "unverified kinds are noted");
    }

    /// Paste Special: in-place plus a typed offset, or the clipboard centre at the camera target; one
    /// undo step each time, and the pasted object is selected.
    #[test]
    fn paste_special_places_in_place_with_an_offset_or_at_the_target() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut p = Places::new();
        p.set_session(session());
        p.pick(Some((0x10, 0)), PickMode::Replace);
        p.copy(&ctx);
        let ps = PasteSpecial { at_target: false, keep_tags: false, offset: [1.0, 2.0, 3.0] };
        p.paste_with(&ctx, &ps);
        let pasted = p.session.as_ref().unwrap().entry(0x10).unwrap().objects()[2].position().unwrap();
        assert_eq!(pasted, [2.0, 4.0, 6.0], "in place + the extra offset");
        assert_eq!(p.session.as_ref().unwrap().active().len(), 1, "one paste = one undo step");

        p.pick(Some((0x10, 0)), PickMode::Replace);
        p.copy(&ctx);
        p.view.goto([10.0, 0.0, 0.0], 20.0);
        let ps = PasteSpecial { at_target: true, keep_tags: false, offset: [0.0; 3] };
        p.paste_with(&ctx, &ps);
        let pasted = p.session.as_ref().unwrap().entry(0x10).unwrap().objects()[3].position().unwrap();
        assert_eq!(pasted, [10.0, 0.0, 0.0], "the clipboard centre lands at the camera target");
    }

    /// The live readback refuses explicitly without a game folder or an addressable selection: it must
    /// never write a request into an unknown folder.
    #[test]
    fn live_readback_refuses_without_a_game_folder_or_named_selection() {
        let mut p = Places::new();
        p.set_session(session());
        p.pick(Some((0x10, 0)), PickMode::Replace);
        p.game_dir = String::new();
        p.live_read();
        let (msg, err) = p.status.clone().unwrap();
        assert!(err && msg.contains("no game folder"), "{msg}");
        p.game_dir = std::env::temp_dir().to_string_lossy().to_string();
        p.live_read();
        let (msg, err) = p.status.clone().unwrap();
        assert!(err && msg.contains("no selected object has a recovered Lua path"), "{msg}");
        assert!(!p.live.has_backup());
    }

    /// Ariane-style selection modes, and a multi-object move from the view as one undo step.
    #[test]
    fn selection_modes_and_a_group_move_undo_in_one_step() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut p = Places::new();
        p.set_session(session());
        p.pick(Some((0x10, 0)), PickMode::Replace);
        p.pick(Some((0x20, 0)), PickMode::Add);
        assert_eq!(p.selected(), vec![(0x20, 0), (0x10, 0)], "Add makes the new object primary");
        p.pick(Some((0x20, 0)), PickMode::Toggle);
        assert_eq!(p.selected(), vec![(0x10, 0)]);
        p.pick_many(vec![(0x10, 1), (0x20, 0)], PickMode::Add);
        assert_eq!(p.selected(), vec![(0x10, 0), (0x10, 1), (0x20, 0)]);
        p.pick_many(vec![(0x20, 0), (0x10, 0)], PickMode::Replace);
        assert_eq!(p.selected(), vec![(0x10, 0), (0x20, 0)], "a box that includes the primary keeps it primary");
        p.pick_many(vec![(0x10, 1)], PickMode::Add);
        p.pick_many(vec![(0x10, 0)], PickMode::Remove);
        assert_eq!(p.primary(), Some((0x20, 0)));
        p.pick_many(vec![(0x20, 0)], PickMode::Remove);
        assert_eq!(p.primary(), Some((0x10, 1)));
        p.move_many(&[(0x10, 1, [5.0, 5.0, 6.0]), (0x20, 0, [8.0, 8.0, 9.0])]);
        assert_eq!(p.session.as_ref().unwrap().active().len(), 2);
        p.undo();
        assert_eq!(p.session.as_ref().unwrap().active().len(), 0, "one undo reverts the whole group");
        p.pick(None, PickMode::Replace);
        assert!(p.selected().is_empty());
        frame(&ctx, &mut p, "");
    }
}
