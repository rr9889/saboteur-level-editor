//! Per-`ClassName` parameter schema, observed from the open install's EditNodes data.
//!
//! The game does not ship a machine-readable schema for placed objects: a `ClassName` selects a
//! GameTemplates archetype, and the object's leaves are authored per instance. What this module
//! builds is the *observed* schema — for every ClassName in the effective EditNodes data, which
//! parameter tags its objects carry and how often — so the inspector can say "this class usually
//! has these parameters" and the ClassName picker can show classes that this install actually uses.
//!
//! That is an observation, not a format requirement: extra parameters are legal, and a class seen
//! once proves little. The UI therefore labels the section "observed", never "valid".

use std::collections::{BTreeMap, BTreeSet};

use sab_formats::editnodes::tags;
use sab_tools::session::Session;

/// What every object of one ClassName carried in this install's data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassSchema {
    /// Objects of this class in the effective data.
    pub objects: usize,
    /// Parameter tag → number of objects that carry it.
    pub params: BTreeMap<u32, usize>,
}

impl ClassSchema {
    /// Parameter tags seen on every one of this class's objects.
    pub fn common(&self) -> BTreeSet<u32> {
        self.params.iter().filter(|(_, n)| **n == self.objects).map(|(t, _)| *t).collect()
    }
}

/// The observed schema of one install.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub classes: BTreeMap<String, ClassSchema>,
    pub objects: usize,
}

impl Schema {
    /// Scan every object in the effective data (DLC/01 over base) once.
    pub fn build(s: &Session) -> Schema {
        let mut classes: BTreeMap<String, ClassSchema> = BTreeMap::new();
        let mut objects = 0usize;
        for c in s.containers() {
            let Some(e) = s.entry(c.hash) else { continue };
            for o in e.objects() {
                let Some(class) = o.class_name().filter(|c| !c.trim().is_empty()) else { continue };
                objects += 1;
                let entry = classes.entry(class).or_default();
                entry.objects += 1;
                for tag in o.params().iter().map(|p| p.tag()).filter(|t| *t != tags::CLASS_NAME) {
                    *entry.params.entry(tag).or_default() += 1;
                }
            }
        }
        Schema { classes, objects }
    }

    pub fn class(&self, name: &str) -> Option<&ClassSchema> {
        self.classes.get(name)
    }

    /// Classes by descending object count, then name; the ClassName picker's order.
    pub fn ranked(&self) -> Vec<(&str, usize)> {
        let mut v: Vec<(&str, usize)> = self.classes.iter().map(|(k, c)| (k.as_str(), c.objects)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        v
    }

    /// The class's parameter tags, whether this object's tag is one of them, and the class's sample
    /// count. `None` for an unknown class.
    pub fn known(&self, class: &str) -> Option<usize> {
        self.class(class).map(|c| c.objects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sab_formats::editnodes::{Entry, Node, Pack, Root};
    use std::path::Path;
    use sab_tools::stage::EditNodesSources;

    fn object(tag: u32, class: &str, params: &[u32]) -> Node {
        let mut children = vec![Node::Leaf { tag: tags::CLASS_NAME, data: format!("{class}\0").into_bytes() }];
        for p in params {
            children.push(Node::Leaf { tag: *p, data: vec![1, 2, 3, 4] });
        }
        Node::Container { tag, children }
    }

    fn session() -> Session {
        let base = Pack {
            entries: vec![Entry {
                hash: 0x10,
                root: Root::EditNode {
                    objects: vec![
                        object(0xA, "Locator", &[tags::POSITION]),
                        object(0xB, "Locator", &[tags::POSITION, tags::LUA_PARAM]),
                        object(0xC, "Prop", &[tags::POSITION]),
                    ],
                },
            }],
        };
        let dlc = Pack { entries: vec![] };
        Session::from_sources(Path::new("G"), EditNodesSources::from_bytes(base.write(), dlc.write(), "LF".into()).unwrap())
    }

    #[test]
    fn the_observed_schema_counts_objects_and_params_per_class() {
        let s = Schema::build(&session());
        assert_eq!(s.objects, 3);
        assert_eq!(s.classes.len(), 2);
        let locator = s.class("Locator").unwrap();
        assert_eq!((locator.objects, locator.params[&tags::POSITION], locator.params[&tags::LUA_PARAM]), (2, 2, 1));
        assert_eq!(locator.common(), BTreeSet::from([tags::POSITION]), "LuaParam is on only one of the two");
        assert_eq!(s.class("Missing"), None);
        assert_eq!(s.known("Locator"), Some(2));
        assert_eq!(s.known("Missing"), None);
        assert_eq!(s.ranked()[0], ("Locator", 2), "ranked by use, then name");
    }
}
