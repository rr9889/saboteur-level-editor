//! Retail gate for `editnodes`: the main `EditNodes.pack` (inside `France/loosefiles_BinPC.pack`) and
//! the DLC one parse, write back byte for byte, and every leaf tagged `tags::LUA_PARAM` decodes with
//! `lua_param`.
//!
//! Game-gated: built by the `retail` feature, reads the install named by the repo-root
//! `.sab-local.toml`, and fails if it is absent.

use std::path::{Path, PathBuf};

use sab_formats::editnodes::{lua_param, lua_param_bytes, tags, LuaValue, Node, Pack, Root};
use sab_formats::game_paths::local_config_retail_dir;
use sab_formats::loosefiles;

fn retail() -> PathBuf {
    local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"))
}

fn leaves<'a>(r: &'a Root, out: &mut Vec<&'a Node>) {
    fn node<'a>(n: &'a Node, out: &mut Vec<&'a Node>) {
        match n {
            Node::Leaf { .. } => out.push(n),
            Node::Container { children, .. } => children.iter().for_each(|c| node(c, out)),
        }
    }
    match r {
        Root::EditNode { objects } => objects.iter().for_each(|o| node(o, out)),
        Root::Container { children, .. } => children.iter().for_each(|c| leaves(c, out)),
    }
}

/// (LuaParam leaves, LuaCrcList items) in a pack that must round-trip.
fn check(label: &str, bytes: &[u8]) -> (usize, usize) {
    let pack = Pack::parse(bytes).unwrap_or_else(|e| panic!("{label}: {e}"));
    assert!(pack.write() == bytes, "{label}: rebuild differs");
    let (mut params, mut items) = (0, 0);
    for e in &pack.entries {
        let mut ls = Vec::new();
        leaves(&e.root, &mut ls);
        for n in ls {
            let Node::Leaf { tag, data } = n else { continue };
            if *tag == tags::LUA_PARAM {
                let p = lua_param(data).unwrap_or_else(|err| panic!("{label} entry 0x{:08x}: {err}", e.hash));
                assert!(&lua_param_bytes(&p) == data, "{label} entry 0x{:08x}: LuaParam '{}' re-encodes differently", e.hash, p.name);
                params += 1;
                if let LuaValue::CrcList(v) = &p.value {
                    items += v.len();
                }
            }
        }
    }
    (params, items)
}

#[test]
fn retail_editnodes_round_trip_and_every_lua_param_decodes() {
    let retail = retail();
    let lf = std::fs::read(retail.join("France/loosefiles_BinPC.pack")).expect("loosefiles_BinPC.pack");
    let es = loosefiles::entries(&lf).expect("loosefiles walk");
    let main = loosefiles::find(&es, r"France\EditNodes\EditNodes.pack").expect("main EditNodes.pack");
    assert_eq!(check("main", &lf[main.data.clone()]), (4548, 775));
    let dlc = std::fs::read(retail.join("DLC/01/France/EditNodes/EditNodes.pack")).expect("DLC EditNodes.pack");
    assert_eq!(check("DLC 01", &dlc), (5, 0));
}

/// The stored rotation basis: how many objects carry XAxis/YAxis/ZAxis, their handedness
/// (sign of (X × Y) · Z) and how far they are from orthonormal. An observation about retail data
/// that the editor's rotation relies on, printed for the docs; not a format rule.
#[test]
fn retail_basis_handedness_and_orthonormality() {
    let retail = retail();
    let lf = std::fs::read(retail.join("France/loosefiles_BinPC.pack")).expect("loosefiles_BinPC.pack");
    let es = loosefiles::entries(&lf).expect("loosefiles walk");
    let main = loosefiles::find(&es, r"France\EditNodes\EditNodes.pack").expect("main EditNodes.pack");
    let pack = Pack::parse(&lf[main.data.clone()]).expect("parse");
    let (mut right, mut left, mut odd, mut worst) = (0usize, 0usize, 0usize, 0f32);
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let cross = |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    for e in &pack.entries {
        for o in e.objects() {
            let (Some(x), Some(y), Some(z)) = (o.vec3_param(tags::X_AXIS), o.vec3_param(tags::Y_AXIS), o.vec3_param(tags::Z_AXIS)) else { continue };
            let dev = [dot(x, x) - 1.0, dot(y, y) - 1.0, dot(z, z) - 1.0, dot(x, y), dot(y, z), dot(z, x)].iter().fold(0f32, |m, v| m.max(v.abs()));
            worst = worst.max(dev);
            let h = dot(cross(x, y), z);
            if h > 0.99 {
                right += 1;
            } else if h < -0.99 {
                left += 1;
            } else {
                odd += 1;
            }
        }
    }
    println!("basis: right-handed {right}, left-handed {left}, neither {odd}; worst orthonormality deviation {worst:e}");
    assert!(right > 0, "no basis found");
    assert_eq!(left + odd, 0, "every retail basis observed so far is right-handed (X × Y = Z)");
}
