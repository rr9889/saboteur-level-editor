//! The area browser's data: named destinations a user can open in the placement editor.
//!
//! Saboteur has no Fallout-style cell records. What it has, and what this module joins:
//! - `InteriorManager.InteriorList` (shipped Lua, `docs/saboteur-luacd/src/Managers/InteriorManager.lua`):
//!   twelve named interiors, each with a script and EditNodes locator paths for its interior teleport
//!   point, exterior teleport point and exterior map blip. [`INTERIORS`] is that table, copied by
//!   hand; the retail test checks every path against the install.
//! - EditNodes (`Session`): the locators' positions, and every other placed object.
//! - `France.map` (lists 2 and 3): named map objects with bounding boxes. A room's floor and walls
//!   live in one of these (`hq_int` for Belle), not in the surrounding terrain cells.
//!
//! Three things are kept apart: the *area* (what the user picks), its *resources* (the map objects
//! and EditNodes objects shown for it) and its *camera anchor* (where the view starts). Nothing here
//! assumes the interior list is complete, and no grid or streaming rule is invented: an exterior
//! bookmark reports the map objects whose box contains it and nothing more.

use std::path::Path;

use sab_formats::editnodes;
use sab_formats::map6;

use crate::session::Session;

/// What kind of destination an area is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An entry of `InteriorManager.InteriorList`.
    Interior,
    /// An interior's exterior entrance (its map blip), a named bookmark in the street.
    Exterior,
    /// A small `France.map` object no interior links to (cinematic sets, props, mission pieces).
    Technical,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Interior => "interior",
            Kind::Exterior => "exterior",
            Kind::Technical => "technical",
        }
    }
}

/// A `France.map` object (list 2 or 3) small enough to be a room, with its box.
#[derive(Clone, Debug)]
pub struct MapObject {
    pub hash: u32,
    pub name: String,
    pub a: [f32; 3],
    pub b: [f32; 3],
}

impl MapObject {
    pub fn contains(&self, p: [f32; 3], margin: f32) -> bool {
        (0..3).all(|k| p[k] >= self.a[k].min(self.b[k]) - margin && p[k] <= self.a[k].max(self.b[k]) + margin)
    }

    pub fn centre(&self) -> [f32; 3] {
        [0, 1, 2].map(|k| (self.a[k] + self.b[k]) / 2.0)
    }

    pub fn volume(&self) -> f32 {
        (0..3).map(|k| (self.a[k] - self.b[k]).abs()).product()
    }
}

/// An EditNodes object, by container and index (the editor's `ObjRef`), with what the list shows.
/// `tag` is the object's stable identity: an edit can move it to another index, the tag does not move.
#[derive(Clone, Debug)]
pub struct AreaObject {
    pub obj: (u32, usize),
    /// The object tag (its stable identity across index changes).
    pub tag: u32,
    pub name: String,
    pub class_name: String,
    pub pos: [f32; 3],
}

/// One destination in the browser.
#[derive(Clone, Debug)]
pub struct Area {
    pub name: String,
    pub kind: Kind,
    /// The interior's script name (empty for other kinds).
    pub script: String,
    /// The camera anchor and where it came from (or why there is none).
    pub anchor: Option<[f32; 3]>,
    pub anchor_note: String,
    /// The EditNodes object the anchor is, when it is one.
    pub anchor_obj: Option<(u32, usize)>,
    /// The map object holding the room's geometry, when one is identified, and how.
    pub primary: Option<MapObject>,
    pub primary_note: String,
    /// Every small map object whose box contains the anchor (the primary included).
    pub map_objects: Vec<MapObject>,
    /// EditNodes objects shown for the area: inside the primary's box, or near an exterior anchor.
    pub objects: Vec<AreaObject>,
}

impl Area {
    /// The EditNodes containers the area's objects come from, each once, with its object count.
    /// The container list is what `Session::stage` will export, so it is shown as a resource.
    pub fn containers(&self) -> Vec<(u32, usize)> {
        let mut out: Vec<(u32, usize)> = Vec::new();
        for o in &self.objects {
            match out.iter_mut().find(|(c, _)| *c == o.obj.0) {
                Some((_, n)) => *n += 1,
                None => out.push((o.obj.0, 1)),
            }
        }
        out.sort_unstable_by_key(|(c, _)| *c);
        out
    }

    /// One line for the list: what is available to draw and edit.
    pub fn availability(&self) -> String {
        match (&self.primary, self.map_objects.len(), self.anchor) {
            (_, _, None) => "no anchor: cannot open".to_string(),
            (Some(p), _, _) => format!("room geometry: {} · {} objects", p.name, self.objects.len()),
            (None, n, _) if n > 0 => format!("{n} map object(s) here · {} objects", self.objects.len()),
            _ if self.kind == Kind::Interior => format!("no room geometry found for this interior · {} objects nearby", self.objects.len()),
            _ => format!("outdoor scenery: terrain cells · {} objects nearby", self.objects.len()),
        }
    }
}

/// One `InteriorList` entry: the paths the Lua gives. Empty strings are entries the Lua leaves out.
pub struct InteriorDef {
    pub name: &'static str,
    pub script: &'static str,
    /// `sIntTeleLoc`: the EditNodes locator the player is placed at inside.
    pub int_tele: &'static str,
    /// `sExteriorLoc`: the exterior map-blip locator.
    pub ext_blip: &'static str,
}

/// `InteriorManager.InitList` (12 entries). `Pantheon` and `Zeppelin` give no usable path for one of
/// the two locators; that is the Lua's, not an omission here.
pub const INTERIORS: [InteriorDef; 12] = [
    InteriorDef { name: "RedOx", script: "RedOx_Interior", int_tele: r"CountrySide\alsace\town\interior\redox_int\LOC_RO_Int", ext_blip: r"CountrySide\alsace\town\interior\redox_ext\ext_blip_loc\LOC_RO_Blip_Ext" },
    InteriorDef { name: "Belle", script: "Belle_Interior", int_tele: r"PARIS\area01\belledenuit\interior\hq_int\LOC_Belle_int", ext_blip: r"PARIS\area01\belledenuit\interior\hq_ext\ext_blip_loc\LOC_Belle_ext" },
    InteriorDef { name: "Belle_Destroyed", script: "Belle_Interior_Destroyed", int_tele: r"PARIS\area01\belledenuit\interior\int_belledestoyed\LOC_Int_Tele1", ext_blip: r"PARIS\area01\belledenuit\interior\hq_ext\ext_blip_loc\LOC_Belle_ext" },
    InteriorDef { name: "LaVillette", script: "LaVillette_Interior", int_tele: r"PARIS\area01\lavillette\interior\lavillette_int\LOC_LV_Int", ext_blip: r"PARIS\area01\lavillette\interior\lavillette_ext\ext_blip_loc\LOC_LV_Blip_Ext" },
    InteriorDef { name: "HDV", script: "Hotel_Interior", int_tele: r"PARIS\area06\hoteldeville\interior\hdv_int\LOC_LV_Int", ext_blip: r"PARIS\area06\hoteldeville\interior\hdv_ext\ext_blip_loc\LOC_Hotel_Ext" },
    InteriorDef { name: "Boulogne", script: "Boulogne_Interior", int_tele: r"PARIS\area02\boisdeboulogne\hq\boulogne_int\LOC_BOL_Int", ext_blip: r"PARIS\area02\boisdeboulogne\hq\boulogne_ext\ext_blip_loc\LOC_BOL_Blip_Ext" },
    InteriorDef { name: "Catacombs", script: "Catacombs_Interior", int_tele: r"PARIS\area03\catacombs\hq\LOC_Cat_Int", ext_blip: r"PARIS\area03\catacombs\catacombshq_ex\ext_blip_loc\LOC_Cat_Blip_Ext" },
    InteriorDef { name: "LeHavre", script: "LeHavreHQ_Interior", int_tele: r"LeHavre\lehavre_hq\LOC_LV_Int", ext_blip: r"LeHavre\lehavre_hq_ext\ext_blip_loc\LOC_LH_Blip_Ext" },
    InteriorDef { name: "LeHavreHotel", script: "LeHavreHotel_Interior", int_tele: r"LeHavre\int_hotel_skylar\LOC_LH_Int", ext_blip: r"LeHavre\lehavre_hotel_ext\ext_blip_loc\LOC_LHT_Blip_Ext" },
    InteriorDef { name: "Zeppelin", script: "Zeppelin_Int", int_tele: r"Missions\soe_1\zeppelin\main_zep\LOC_IntZepTele", ext_blip: "" },
    InteriorDef { name: "Pantheon", script: "Pantheon_Interior", int_tele: "", ext_blip: "" },
    InteriorDef { name: "SaarHQ", script: "SaarHQ_Interior", int_tele: r"CountrySide\alsace\town\interior\int_saar_hotel\LOC_SaarHQ_Int", ext_blip: r"CountrySide\alsace\town\interior\int_saar_hotel_ext\ext_blip_loc\LOC_Saar_Blip_Ext" },
];

/// Where the player actually landed in game after `InteriorManager.EnterInterior` / `ExitInterior`, measured 2026-11-03
/// (docs/teleport_streaming/, `Object.GetPosition` readback; evidence: observed in game, one run each). `None` = not verified:
/// HDV, Pantheon and Zeppelin have no default exit/entry locator, Belle_Destroyed was not attempted. These are the ONLY
/// in-game coordinates in this module; they are navigation hints for the editor, never writes to game data.
pub struct VerifiedLanding {
    pub interior: &'static str,
    /// Position after entering the interior (inside the interior volume, which sits above its exterior).
    pub inside: Option<[f32; 3]>,
    /// Position after leaving it by the default exit.
    pub outside: Option<[f32; 3]>,
    pub note: &'static str,
}

pub const VERIFIED_LANDINGS: [VerifiedLanding; 12] = [
    VerifiedLanding { interior: "Belle", inside: Some([-65.97, 260.04, -752.73]), outside: Some([-74.75, 71.18, -753.25]), note: "entered and exited in game" },
    VerifiedLanding { interior: "LaVillette", inside: Some([768.58, 293.46, -725.85]), outside: Some([777.62, 52.73, -716.37]), note: "entered and exited in game" },
    VerifiedLanding { interior: "Boulogne", inside: Some([-1375.58, 206.84, -564.07]), outside: Some([-1367.89, 56.53, -541.90]), note: "entered and exited in game" },
    VerifiedLanding { interior: "LeHavre", inside: Some([-1019.50, 310.04, -2153.37]), outside: Some([-1003.90, 27.52, -2140.44]), note: "entered and exited in game" },
    VerifiedLanding { interior: "LeHavreHotel", inside: Some([-1089.24, 310.04, -2111.04]), outside: Some([-1080.47, 5.24, -2110.13]), note: "entered and exited in game" },
    VerifiedLanding { interior: "SaarHQ", inside: Some([2483.45, 250.04, -2635.25]), outside: Some([2588.07, 64.75, -2665.17]), note: "entered and exited in game" },
    VerifiedLanding { interior: "Catacombs", inside: Some([-680.99, 207.99, 553.35]), outside: Some([-591.85, 46.96, 554.92]), note: "entered and exited in game" },
    VerifiedLanding { interior: "RedOx", inside: Some([2629.17, 200.38, -2494.53]), outside: Some([2627.33, 41.22, -2445.42]), note: "entered and exited in game" },
    VerifiedLanding { interior: "HDV", inside: Some([84.38, 249.33, 117.29]), outside: None, note: "entered in game; no default exit locator, exit not verified" },
    VerifiedLanding { interior: "Zeppelin", inside: None, outside: None, note: "not verified: no default entry/exit locator" },
    VerifiedLanding { interior: "Pantheon", inside: None, outside: None, note: "not verified: no default entry/exit locator" },
    VerifiedLanding { interior: "Belle_Destroyed", inside: None, outside: None, note: "not attempted" },
];

/// The measured landing for an area: an interior by its name, or an "<interior> entrance" exterior.
pub fn verified_landing(area_name: &str) -> Option<&'static VerifiedLanding> {
    let base = area_name.strip_suffix(" entrance").unwrap_or(area_name);
    VERIFIED_LANDINGS.iter().find(|l| l.interior.eq_ignore_ascii_case(base))
}

/// Indices of the areas matching a search: every whitespace-separated word must appear in the name, script name or kind label
/// (case-insensitive). An empty query matches everything. Pure, so it is tested without a game install.
pub fn search(areas: &[Area], query: &str) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(|w| w.to_ascii_lowercase()).collect();
    areas
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            let hay = format!("{} {} {}", a.name, a.script, a.kind.label()).to_ascii_lowercase();
            words.iter().all(|w| hay.contains(w))
        })
        .map(|(i, _)| i)
        .collect()
}

/// One Go to Object result. `pos` is `None` when the object has no usable transform (nothing to frame).
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectHit {
    pub obj: (u32, usize),
    pub tag: u32,
    pub name: String,
    pub class_name: String,
    pub pos: Option<[f32; 3]>,
}

/// Does `q` select this object? Accepted forms (case-insensitive): an 8-digit hex tag (`878ccdfa`, `0x878ccdfa`), a
/// `container#index` reference (container hash in hex, e.g. `1a2b3c4d#17`), or words that must all appear in the name
/// or class name. An empty query matches nothing. Pure: no game or session needed.
pub fn object_matches(obj: (u32, usize), tag: u32, name: &str, class_name: &str, q: &str) -> bool {
    let q = q.trim();
    if q.is_empty() {
        return false;
    }
    let hex = q.strip_prefix("0x").or_else(|| q.strip_prefix("0X")).unwrap_or(q);
    if hex.len() == 8 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        if u32::from_str_radix(hex, 16).map_or(false, |t| t == tag) {
            return true;
        }
    }
    if let Some((c, i)) = q.split_once('#') {
        let c = c.trim().trim_start_matches("0x").trim_start_matches("0X");
        if let (Ok(c), Ok(i)) = (u32::from_str_radix(c, 16), i.trim().parse::<usize>()) {
            return obj == (c, i);
        }
    }
    let hay = format!("{name} {class_name}").to_ascii_lowercase();
    q.split_whitespace().all(|w| hay.contains(&w.to_ascii_lowercase()))
}

/// Search every loaded EditNodes object. Returns up to `limit` hits (stable container/index order) and the total number of
/// matches. Read-only: navigating never touches the edit journal. Objects in containers the session did not load are not
/// searched (the caller says so when nothing matches).
pub fn find_objects(s: &Session, q: &str, limit: usize) -> (Vec<ObjectHit>, usize) {
    let mut hits = Vec::new();
    let mut total = 0;
    for c in s.containers() {
        let Some(e) = s.entry(c.hash) else { continue };
        for (i, o) in e.objects().iter().enumerate() {
            let name = s.object_name(o.tag()).map(|n| n.rsplit('\\').next().unwrap_or(&n).to_string()).unwrap_or_else(|| format!("0x{:08x}", o.tag()));
            let class_name = o.class_name().unwrap_or_default();
            if object_matches((c.hash, i), o.tag(), &name, &class_name, q) {
                total += 1;
                if hits.len() < limit {
                    let pos = o.position().filter(|p| p.iter().all(|v| v.is_finite()));
                    hits.push(ObjectHit { obj: (c.hash, i), tag: o.tag(), name, class_name, pos });
                }
            }
        }
    }
    (hits, total)
}

/// Map objects wider than this on any axis are regions, not rooms (matches the room loader).
pub const MAX_ROOM_EXTENT: f32 = 150.0;

/// How near an exterior bookmark an EditNodes object counts as "nearby" (metres).
pub const NEARBY: f32 = 40.0;

/// Split a Lua locator path `container\path\NAME` into its container path and object name.
pub fn split_locator(path: &str) -> Option<(&str, &str)> {
    path.rsplit_once('\\').filter(|(c, n)| !c.is_empty() && !n.is_empty())
}

/// The EditNodes object a locator path names: its container, index and position, if the install
/// has it. The object's tag is the hash of the full path, its container the hash of the path
/// without the last segment (`editnodes::object_tag` / `container_hash`).
pub fn locate(s: &Session, path: &str) -> Option<((u32, usize), [f32; 3])> {
    let (container_path, name) = split_locator(path)?;
    let c = editnodes::container_hash(container_path);
    let tag = editnodes::object_tag(container_path, name);
    // The tag is the hash of the full path, but the object may sit in a different container than the
    // path suggests: try the named container first, then every container.
    let find = |c: u32| -> Option<((u32, usize), [f32; 3])> {
        let entry = s.entry(c)?;
        let i = entry.objects().iter().position(|o| o.tag() == tag)?;
        Some(((c, i), entry.objects()[i].position()?))
    };
    find(c).or_else(|| s.containers().into_iter().find_map(|k| find(k.hash)))
}

/// Every `France.map` object (lists 2 and 3) that is a room-sized box.
pub fn small_map_objects(game: &Path) -> Result<Vec<MapObject>, String> {
    let lf_path = game.join("France").join("loosefiles_BinPC.pack");
    let lf = std::fs::read(&lf_path).map_err(|e| format!("{}: {e}", lf_path.display()))?;
    let es = sab_formats::loosefiles::entries(&lf)?;
    let fm = sab_formats::loosefiles::find(&es, "France.map").ok_or_else(|| format!("{}: no France.map entry", lf_path.display()))?;
    let rm = map6::parse_region_main(&lf[fm.data.clone()]).map_err(|e| format!("France.map: {e}"))?;
    Ok(rm
        .list2
        .iter()
        .chain(&rm.list3)
        .filter(|r| (0..3).all(|k| (r.a[k] - r.b[k]).abs() <= MAX_ROOM_EXTENT))
        .map(|r| MapObject { hash: r.hash, name: r.name(), a: r.a, b: r.b })
        .collect())
}

fn describe(s: &Session, c: u32, i: usize) -> Option<AreaObject> {
    let o = s.entry(c)?.objects().get(i)?;
    let name = s.object_name(o.tag()).map(|n| n.rsplit('\\').next().unwrap_or(&n).to_string()).unwrap_or_else(|| format!("0x{:08x}", o.tag()));
    Some(AreaObject { obj: (c, i), tag: o.tag(), name, class_name: o.class_name().unwrap_or_default(), pos: o.position()? })
}

/// EditNodes objects with a position satisfying `inside`, nearest the anchor first.
fn objects_where(s: &Session, anchor: [f32; 3], inside: impl Fn([f32; 3]) -> bool) -> Vec<AreaObject> {
    let mut v = Vec::new();
    for c in s.containers() {
        let Some(e) = s.entry(c.hash) else { continue };
        for (i, o) in e.objects().iter().enumerate() {
            if o.position().map_or(false, &inside) {
                v.extend(describe(s, c.hash, i));
            }
        }
    }
    let d2 = |p: [f32; 3]| (0..3).map(|k| (p[k] - anchor[k]).powi(2)).sum::<f32>();
    v.sort_by(|a, b| d2(a.pos).total_cmp(&d2(b.pos)));
    v
}

/// Pick the map object holding a room's geometry: the one named like the interior locator's
/// container (`…\hq_int\LOC_Belle_int` → `hq_int`), else the largest box containing the anchor.
/// Returns it with how it was chosen. The name match is an inference, confirmed by containment.
fn primary_for(int_tele: &str, containing: &[MapObject]) -> (Option<MapObject>, String) {
    let leaf = split_locator(int_tele).and_then(|(c, _)| c.rsplit('\\').next()).unwrap_or("");
    if let Some(m) = containing.iter().find(|m| !leaf.is_empty() && m.name.eq_ignore_ascii_case(leaf)) {
        return (Some(m.clone()), format!("named like the locator's container ({leaf}) and contains the anchor"));
    }
    match containing.iter().max_by(|a, b| a.volume().total_cmp(&b.volume())) {
        Some(m) => (Some(m.clone()), "no map object is named like the locator's container; the largest box containing the anchor".into()),
        None => (None, "no small map object contains the anchor".into()),
    }
}

/// Build the browser's areas for an install: its interiors, their exterior entrances and the
/// remaining room-sized map objects.
pub fn build(game: &Path, s: &Session) -> Result<Vec<Area>, String> {
    let maps = small_map_objects(game)?;
    let mut areas = Vec::new();
    let mut used: Vec<u32> = Vec::new();
    for def in &INTERIORS {
        let located = (!def.int_tele.is_empty()).then(|| locate(s, def.int_tele)).flatten();
        let (anchor, anchor_obj, anchor_note) = match located {
            Some((obj, p)) => (Some(p), Some(obj), format!("{} (EditNodes {:08x} #{})", def.int_tele.rsplit('\\').next().unwrap_or(""), obj.0, obj.1)),
            None if def.int_tele.is_empty() => (None, None, "the interior list names no interior locator".to_string()),
            None => {
                // The interior's teleport locators are not in EditNodes.pack (the room's own content
                // holds them). Fall back to the map object named like the locator's container.
                let leaf = split_locator(def.int_tele).and_then(|(c, _)| c.rsplit('\\').next()).unwrap_or("");
                match maps.iter().find(|m| m.name.eq_ignore_ascii_case(leaf)) {
                    Some(m) => (Some(m.centre()), None, format!("centre of map object {} (the locator {} is not in EditNodes.pack)", m.name, def.int_tele.rsplit('\\').next().unwrap_or(""))),
                    None => (None, None, format!("{} is not in EditNodes.pack and no map object is named {leaf}", def.int_tele)),
                }
            }
        };
        let containing: Vec<MapObject> = anchor.map(|p| maps.iter().filter(|m| m.contains(p, 2.0)).cloned().collect()).unwrap_or_default();
        let (primary, primary_note) = match anchor {
            Some(_) => primary_for(def.int_tele, &containing),
            None => (None, "no anchor".into()),
        };
        used.extend(containing.iter().map(|m| m.hash));
        let objects = match (&primary, anchor) {
            (Some(m), Some(a)) => objects_where(s, a, |p| m.contains(p, 0.0)),
            (None, Some(a)) => objects_where(s, a, |p| (0..3).map(|k| (p[k] - a[k]).powi(2)).sum::<f32>() <= NEARBY * NEARBY),
            _ => Vec::new(),
        };
        areas.push(Area { name: def.name.into(), kind: Kind::Interior, script: def.script.into(), anchor, anchor_note, anchor_obj, primary, primary_note, map_objects: containing, objects });
        // The exterior entrance, once per distinct blip (Belle and its destroyed variant share one).
        if !def.ext_blip.is_empty() && !areas.iter().any(|a| a.kind == Kind::Exterior && a.anchor_note.starts_with(def.ext_blip.rsplit('\\').next().unwrap_or("?"))) {
            if let Some((obj, p)) = locate(s, def.ext_blip) {
                let containing: Vec<MapObject> = maps.iter().filter(|m| m.contains(p, 2.0)).cloned().collect();
                used.extend(containing.iter().map(|m| m.hash));
                let objects = objects_where(s, p, |q| (0..3).map(|k| (q[k] - p[k]).powi(2)).sum::<f32>() <= NEARBY * NEARBY);
                areas.push(Area {
                    name: format!("{} entrance", def.name),
                    kind: Kind::Exterior,
                    script: String::new(),
                    anchor: Some(p),
                    anchor_note: format!("{} (EditNodes {:08x} #{})", def.ext_blip.rsplit('\\').next().unwrap_or(""), obj.0, obj.1),
                    anchor_obj: Some(obj),
                    primary: None,
                    primary_note: "street geometry loads from the effective terrain cells".into(),
                    map_objects: containing,
                    objects,
                });
            }
        }
    }
    // Room-sized map objects no interior reached.
    for m in &maps {
        let flat = (0..3).any(|k| (m.a[k] - m.b[k]).abs() < 0.5);
        if used.contains(&m.hash) || flat {
            continue;
        }
        let c = m.centre();
        areas.push(Area {
            name: m.name.clone(),
            kind: Kind::Technical,
            script: String::new(),
            anchor: Some(c),
            anchor_note: "centre of the map object's box".into(),
            anchor_obj: None,
            primary: Some(m.clone()),
            primary_note: "the map object itself".into(),
            map_objects: vec![m.clone()],
            objects: objects_where(s, c, |p| m.contains(p, 0.0)),
        });
    }
    Ok(areas)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare(name: &str, script: &str, kind: Kind) -> Area {
        Area { name: name.into(), kind, script: script.into(), anchor: None, anchor_note: String::new(), anchor_obj: None, primary: None, primary_note: String::new(), map_objects: Vec::new(), objects: Vec::new() }
    }

    #[test]
    fn search_matches_every_word_in_name_script_or_kind_and_empty_matches_all() {
        let a = vec![bare("Belle", "Belle_Interior", Kind::Interior), bare("Belle entrance", "", Kind::Exterior), bare("LeHavre", "LeHavreHQ_Interior", Kind::Interior)];
        assert_eq!(search(&a, ""), vec![0, 1, 2]);
        assert_eq!(search(&a, "belle"), vec![0, 1]);
        assert_eq!(search(&a, "BELLE interior"), vec![0], "all words must match; kind label counts");
        assert_eq!(search(&a, "hq"), vec![2], "script name is searched");
        assert!(search(&a, "nonexistent area").is_empty(), "no match must not panic");
    }

    #[test]
    fn verified_landings_cover_every_interior_exactly_once_and_flag_the_unverified() {
        for def in &INTERIORS {
            assert_eq!(VERIFIED_LANDINGS.iter().filter(|l| l.interior == def.name).count(), 1, "{} needs exactly one landing row", def.name);
        }
        assert_eq!(verified_landing("Belle").unwrap().inside, Some([-65.97, 260.04, -752.73]));
        assert_eq!(verified_landing("Belle entrance").unwrap().outside, Some([-74.75, 71.18, -753.25]), "exterior entrances use their interior's row");
        assert!(verified_landing("HDV").unwrap().outside.is_none(), "HDV has no default exit: not invented");
        assert!(verified_landing("Pantheon").unwrap().inside.is_none());
        assert!(verified_landing("not an area").is_none());
        for l in &VERIFIED_LANDINGS {
            if let (Some(i), Some(o)) = (l.inside, l.outside) {
                assert!(i[1] > o[1], "{}: interior volumes sit above their exteriors", l.interior);
            }
        }
    }

    #[test]
    fn go_to_object_matches_tag_container_index_and_words_and_nothing_for_empty() {
        let o = (0x1a2b3c4d_u32, 17usize);
        assert!(object_matches(o, 0x878ccdfa, "P_INT_chair2(3)", "P_INT_chair2", "878CCDFA"), "tag, any case");
        assert!(object_matches(o, 0x878ccdfa, "x", "y", "0x878ccdfa"), "tag with 0x");
        assert!(!object_matches(o, 0x878ccdfb, "x", "y", "878ccdfa"), "a different tag does not match");
        assert!(object_matches(o, 1, "x", "y", "1a2b3c4d#17") && !object_matches(o, 1, "x", "y", "1a2b3c4d#18"), "container#index is exact");
        assert!(object_matches(o, 1, "Spore_WM_Officer_BELLEONLY", "Spore_WM", "officer belle"), "all words, name");
        assert!(object_matches(o, 1, "n", "P_INT_RoundTable_Big", "roundtable"), "class name is searched");
        assert!(!object_matches(o, 1, "n", "c", ""), "empty query matches nothing");
        assert!(!object_matches(o, 1, "n", "c", "zzz#qq"), "malformed reference does not panic or match");
    }

    #[test]
    fn locator_paths_split_into_container_and_name() {
        assert_eq!(split_locator(r"PARIS\area01\belledenuit\interior\hq_int\LOC_Belle_int"), Some((r"PARIS\area01\belledenuit\interior\hq_int", "LOC_Belle_int")));
        assert_eq!(split_locator("NOSEPARATOR"), None);
        assert_eq!(split_locator(r"\x"), None);
    }

    fn obj(name: &str, a: [f32; 3], b: [f32; 3]) -> MapObject {
        MapObject { hash: 1, name: name.into(), a, b }
    }

    #[test]
    fn the_primary_map_object_prefers_the_container_name_then_the_largest_box() {
        let small = obj("noncinematic", [0.0; 3], [10.0, 5.0, 10.0]);
        let room = obj("HQ_INT", [0.0; 3], [8.0, 4.0, 8.0]);
        let (p, note) = primary_for(r"A\B\hq_int\LOC", &[small.clone(), room]);
        assert_eq!(p.unwrap().name, "HQ_INT", "{note}");
        let (p, note) = primary_for(r"A\B\other\LOC", &[obj("a", [0.0; 3], [1.0; 3]), small]);
        assert_eq!(p.unwrap().name, "noncinematic", "{note}: the larger box wins");
        assert!(primary_for(r"A\B\hq_int\LOC", &[]).0.is_none());
    }

    #[test]
    fn a_box_contains_a_point_with_a_margin_and_reports_its_centre_and_volume() {
        let m = obj("x", [0.0, 0.0, 0.0], [10.0, 4.0, 2.0]);
        assert!(m.contains([5.0, 2.0, 1.0], 0.0) && !m.contains([10.5, 2.0, 1.0], 0.0) && m.contains([10.5, 2.0, 1.0], 1.0));
        assert_eq!((m.centre(), m.volume()), ([5.0, 2.0, 1.0], 80.0));
    }

    #[test]
    fn an_area_lists_each_container_once_with_its_object_count() {
        let mut area = Area {
            name: "x".into(),
            kind: Kind::Interior,
            script: String::new(),
            anchor: Some([0.0; 3]),
            anchor_note: String::new(),
            anchor_obj: None,
            primary: None,
            primary_note: String::new(),
            map_objects: Vec::new(),
            objects: Vec::new(),
        };
        for (i, c) in [0x10u32, 0x20, 0x10].into_iter().enumerate() {
            area.objects.push(AreaObject { obj: (c, i), tag: 0x100 + i as u32, name: format!("o{i}"), class_name: "C".into(), pos: [0.0; 3] });
        }
        assert_eq!(area.containers(), vec![(0x10, 2), (0x20, 1)]);
        assert_eq!(area.objects[2].tag, 0x102, "the tag is the object's stable identity");
    }
}
