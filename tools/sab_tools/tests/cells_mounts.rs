use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::MmapMut;
use sab_formats::editnodes::tags;
use sab_formats::{map6, megapack, sbla};
use sab_tools::cells::{self, Mount, DLC_MEGAPACK, DLC_PRIORITY};

type Item = (u32, u32, Vec<u8>);

fn pack(items: &[Item]) -> Vec<u8> {
    megapack::write(&items.iter().map(|(crc, index, data)| megapack::Item { crc: *crc, index: *index, data }).collect::<Vec<_>>())
}

fn mount(rel: &str, priority: Option<u32>, items: &[Item]) -> Mount {
    let bytes = pack(items);
    let mut mapped = MmapMut::map_anon(bytes.len()).unwrap();
    mapped.copy_from_slice(&bytes);
    Mount { rel: rel.into(), priority, pack: megapack::parse(&mapped).unwrap(), map: mapped.make_read_only().unwrap() }
}

fn cell(objects: &[(u32, Option<[f32; 3]>)], layer: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&(objects.len() as u32).to_le_bytes());
    for (tag, position) in objects {
        body.extend_from_slice(&tag.to_le_bytes());
        body.extend_from_slice(&u32::from(position.is_some()).to_le_bytes());
        if let Some(position) = position {
            body.extend_from_slice(&tags::POSITION.to_le_bytes());
            body.extend_from_slice(&12u32.to_le_bytes());
            for coordinate in position {
                body.extend_from_slice(&coordinate.to_le_bytes());
            }
        }
    }
    let mut counts = [0; 9];
    counts[sbla::TYPE_STREAMED] = 1;
    counts[sab_formats::cellplace::TYPE_PREFIX] = 1;
    let descriptor = sbla::Descriptor {
        heightfield: None,
        tail: map6::Tail { list1: vec![], list2: vec![], x: 0, counts, a: vec![], nb: 0, b: vec![], c: vec![] },
    };
    let bytes = sbla::write(Some(&descriptor), &[
        sbla::Blob { kind: sbla::TYPE_STREAMED, hash: 0, uncomp: body.len() as u32, f10: 17, f14: layer, data: &body, empty_offset: 0 },
        sbla::Blob { kind: sab_formats::cellplace::TYPE_PREFIX, hash: 1, uncomp: body.len() as u32, f10: 0, f14: 0, data: &body, empty_offset: 0 },
    ]).unwrap();
    assert!(sbla::descriptor_len(&bytes).unwrap() < 0xB4);
    bytes
}

struct Install {
    root: PathBuf,
}

impl Install {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!("sab_mounts_{}_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&root).unwrap();
        Self { root }
    }

    fn put(&self, rel: &str, items: &[Item]) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, pack(items)).unwrap();
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        assert_eq!(self.root.parent(), Some(std::env::temp_dir().as_path()));
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn resolve_entry_uses_crc_and_priority_not_mount_order_or_index() {
    let crc = cells::cell_crc(132609);
    let mounts = vec![
        mount(r"France\Mega1.megapack", Some(40), &[(crc, 132609, vec![1])]),
        mount(r"France\patchmega.megapack", Some(100_050), &[(crc, 999, vec![2])]),
        mount(DLC_MEGAPACK, Some(DLC_PRIORITY), &[(crc, 0, vec![3])]),
    ];
    let (winner, entry) = cells::resolve_entry(&mounts, crc).unwrap();
    assert_eq!((winner.rel.as_str(), entry.index), (DLC_MEGAPACK, 0));
    assert_eq!(megapack::entry_slice(&winner.map, entry).unwrap(), &[3]);
    assert!(cells::resolve_entry(&mounts, crc ^ 1).is_none());
    let (winner, _) = cells::resolve_entry(&mounts[..2], crc).unwrap();
    assert_eq!(winner.priority, Some(100_050));
}

#[test]
fn resolve_and_scan_keep_first_equal_priority_mount_and_first_duplicate_entry() {
    let id = 132609;
    let crc = cells::cell_crc(id);
    let first = cell(&[(7, Some([1.0, 2.0, 3.0]))], 0);
    let other = cell(&[(8, Some([4.0, 5.0, 6.0]))], 0);
    let mounts = vec![
        mount("unknown", None, &[(crc, 0, other.clone())]),
        mount(r"France\patchmega.megapack", Some(100_050), &[(crc, 0, first), (crc, id, other.clone())]),
        mount("tied", Some(100_050), &[(crc, 0, other.clone())]),
        mount(r"France\Mega0.megapack", Some(50), &[(crc, id, other)]),
    ];
    let (winner, entry) = cells::resolve_entry(&mounts, crc).unwrap();
    assert_eq!(winner.rel, r"France\patchmega.megapack");
    assert_eq!(entry.index, 0);
    let (placements, counts) = cells::scan_cells(&mounts).unwrap();
    assert_eq!(placements.len(), 1);
    assert_eq!((placements[0].tag, placements[0].cell, placements[0].pos), (7, id, [1.0, 2.0, 3.0]));
    assert_eq!(counts.get(&7), Some(&1));
    assert!(!counts.contains_key(&8));
}

#[test]
fn scan_uses_effective_bytes_once_preserving_base_ids_and_all_layer_counts() {
    let first_id = 132609;
    let second_id = 1700865;
    let first_crc = cells::cell_crc(first_id);
    let second_crc = cells::cell_crc(second_id);
    let base = cell(&[(1, Some([0.0; 3]))], 0);
    let dlc = cell(&[(7, Some([1.0, 2.0, 3.0])), (7, Some([4.0, 5.0, 6.0])), (9, None)], 0xF8AC_8685);
    let patch = cell(&[(8, Some([10.0, 11.0, 12.0]))], 123);
    let mounts = vec![
        mount(r"France\Mega1.megapack", Some(40), &[(first_crc, first_id, base.clone()), (second_crc, second_id, base.clone()), (first_crc, first_id, base.clone())]),
        mount(r"France\patchmega.megapack", Some(100_050), &[(first_crc, 0, base.clone()), (second_crc, 0, patch)]),
        mount(DLC_MEGAPACK, Some(DLC_PRIORITY), &[(first_crc, 0, dlc), (0xABC, 0, b"not a cell".to_vec()), (cells::cell_crc(999), 999, base.clone()), (0xDEF, 0, base.clone())]),
        mount(r"France\Mega2.megapack", Some(30), &[(first_crc, first_id, base.clone()), (cells::cell_crc(256), 256, base.clone()),
            (cells::cell_crc(258), 258, base), (123, 0, b"global resource".to_vec())]),
    ];
    let (placements, counts) = cells::scan_cells(&mounts).unwrap();
    assert_eq!(placements.len(), 3);
    assert_eq!(counts.len(), 3);
    assert_eq!((counts[&7], counts[&8], counts[&9]), (2, 1, 1));
    for (index, placement) in placements[..2].iter().enumerate() {
        assert_eq!((placement.cell, placement.pack.as_str()), (first_id, DLC_MEGAPACK));
        assert_eq!((placement.nth_in_cell, placement.list, placement.lists_in_cell), (index, 0, 1));
        assert_eq!((placement.list_f10, placement.list_f14, placement.in_prefix), (17, 0xF8AC_8685, true));
    }
    assert_eq!(placements[0].pos, [1.0, 2.0, 3.0]);
    assert_eq!(placements[2].cell, second_id);
    assert_eq!(placements[2].pack, r"France\patchmega.megapack");
    assert_eq!(placements[2].pos, [10.0, 11.0, 12.0]);
    assert_eq!(placements[2].list_f14, 123);
}

#[test]
fn shadowed_bytes_are_not_parsed_and_invalid_winners_do_not_fall_back() {
    let id = 132609;
    let crc = cells::cell_crc(id);
    let valid = cell(&[(7, Some([1.0; 3]))], 0);
    let mounts = vec![
        mount(r"France\Mega0.megapack", Some(50), &[(crc, id, b"bad base".to_vec())]),
        mount(DLC_MEGAPACK, Some(DLC_PRIORITY), &[(crc, 0, valid.clone())]),
    ];
    assert_eq!(cells::scan_cells(&mounts).unwrap().0.len(), 1);
    let mounts = vec![
        mount(r"France\Mega0.megapack", Some(50), &[(crc, id, valid)]),
        mount(DLC_MEGAPACK, Some(DLC_PRIORITY), &[(crc, 0, b"bad winner".to_vec())]),
    ];
    let error = cells::scan_cells(&mounts).err().unwrap();
    assert!(error.contains(DLC_MEGAPACK) && error.contains("132609"), "{error}");
    let descriptorless = sbla::write(None, &[]).unwrap();
    let mounts = vec![
        mount(r"France\Mega0.megapack", Some(50), &[(crc, id, cell(&[(7, Some([1.0; 3]))], 0))]),
        mount(DLC_MEGAPACK, Some(DLC_PRIORITY), &[(crc, 0, descriptorless)]),
    ];
    let (placements, counts) = cells::scan_cells(&mounts).unwrap();
    assert!(placements.is_empty() && counts.is_empty());
}

#[test]
fn mounts_sort_dlc_first_prefer_unsuffixed_slot_zero_and_stop_at_gaps() {
    let install = Install::new();
    for rel in [r"France\Mega.megapack", r"France\Mega0.megapack", r"France\Mega1.megapack", r"France\Mega3.megapack",
        r"France\patchmega.megapack", r"France\patchmega0.megapack", r"France\patchmega1.megapack", r"France\patchmega3.megapack", DLC_MEGAPACK] {
        install.put(rel, &[]);
    }
    let mounts = cells::france_mounts(&install.root).unwrap();
    let actual: Vec<_> = mounts.iter().map(|mount| (mount.rel.as_str(), mount.priority)).collect();
    assert_eq!(actual, vec![(DLC_MEGAPACK, Some(100_100)), (r"France\patchmega.megapack", Some(100_050)),
        (r"France\patchmega1.megapack", Some(100_040)), (r"France\Mega.megapack", Some(50)), (r"France\Mega1.megapack", Some(40))]);
    assert_eq!(cells::free_patch_slot(&install.root).unwrap(), ("patchmega2.megapack".into(), 100_030));
}

#[test]
fn absent_slot_zero_ignores_orphans_and_fallbacks_mount_only_contiguous_slots() {
    let install = Install::new();
    install.put(r"France\Mega1.megapack", &[]);
    install.put(r"France\patchmega1.megapack", &[]);
    assert!(cells::france_mounts(&install.root).unwrap().is_empty());
    assert_eq!(cells::free_patch_slot(&install.root).unwrap(), ("patchmega.megapack".into(), 100_050));
    install.put(r"France\Mega0.megapack", &[]);
    install.put(r"France\patchmega0.megapack", &[]);
    let mounts = cells::france_mounts(&install.root).unwrap();
    let actual: Vec<_> = mounts.iter().map(|mount| (mount.rel.as_str(), mount.priority)).collect();
    assert_eq!(actual, vec![(r"France\patchmega0.megapack", Some(100_050)), (r"France\patchmega1.megapack", Some(100_040)),
        (r"France\Mega0.megapack", Some(50)), (r"France\Mega1.megapack", Some(40))]);
}

#[test]
fn free_patch_slot_reports_all_priorities_then_exhaustion() {
    let install = Install::new();
    for (name, priority) in [("patchmega.megapack", 100_050), ("patchmega1.megapack", 100_040),
        ("patchmega2.megapack", 100_030), ("patchmega3.megapack", 100_020)] {
        assert_eq!(cells::free_patch_slot(&install.root).unwrap(), (name.into(), priority));
        install.put(&format!(r"France\{name}"), &[]);
    }
    assert!(cells::free_patch_slot(&install.root).unwrap_err().contains("all four"));
    let mounts = cells::france_mounts(&install.root).unwrap();
    assert_eq!(mounts.iter().map(|mount| mount.priority.unwrap()).collect::<Vec<_>>(), vec![100_050, 100_040, 100_030, 100_020]);
}

#[test]
fn staging_refuses_dlc_or_patch_owners_and_rechecks_before_writing() {
    let install = Install::new();
    let id = 132609;
    let items = [(cells::cell_crc(id), id, cell(&[(7, Some([1.0, 2.0, 3.0]))], 0))];
    install.put(r"France\Mega0.megapack", &items);
    let (edit, mut session) = sab_tools::cellsession::open_cell(&install.root, id).unwrap();
    session.move_object(edit.containers[0], 0, [4.0, 5.0, 6.0]).unwrap();
    install.put(DLC_MEGAPACK, &items);
    let mounts = cells::france_mounts(&install.root).unwrap();
    assert_eq!(mounts.first().unwrap().rel, DLC_MEGAPACK);
    assert!(cells::resolve_entry(&mounts, cells::cell_crc(id)).unwrap().0.priority.unwrap() > cells::free_patch_slot(&install.root).unwrap().1);
    let error = sab_tools::cellsession::open_cell(&install.root, id).err().unwrap();
    assert!(error.contains(DLC_MEGAPACK) && error.contains("100100") && error.contains("refusing DLC authoring"), "{error}");
    let output = install.root.join("staged");
    let error = sab_tools::cellsession::stage_cell(&install.root, &edit, &session, &output).unwrap_err();
    assert!(error.contains("refusing DLC authoring"), "{error}");
    assert!(!output.exists());
    let patch_only = Install::new();
    patch_only.put(r"France\patchmega.megapack", &items);
    assert!(sab_tools::cellsession::open_cell(&patch_only.root, id).is_err());
    let dlc_only = Install::new();
    dlc_only.put(DLC_MEGAPACK, &items);
    assert!(sab_tools::cellsession::open_cell(&dlc_only.root, id).is_err());
}

#[cfg(feature = "retail")]
#[test]
fn retail_cell132609_uses_430_dlc_records_not_414_base_records() {
    let game = sab_formats::game_paths::local_config_retail_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    let mounts = cells::france_mounts(&game).unwrap();
    let id = 132609;
    let crc = cells::cell_crc(id);
    let Some(dlc) = mounts.iter().find(|mount| mount.rel == DLC_MEGAPACK) else {
        eprintln!("DLC absent: cell132609 DLC regression not applicable");
        return;
    };
    let count = |mount: &Mount| {
        let entry = mount.find(crc).unwrap();
        let sub = megapack::entry_slice(&mount.map, entry).unwrap();
        let layout = sab_formats::cellplace::read(sub).unwrap().unwrap();
        let objects: Vec<_> = layout.lists.iter().flat_map(|list| &list.objects).collect();
        (objects.len(), objects.iter().filter(|object| object.params.iter().any(|param| param.tag == tags::POSITION && param.len == 12)).count())
    };
    let base = mounts.iter().find(|mount| mount.priority == Some(40) && mount.find(crc).is_some()).unwrap();
    eprintln!("cell {id} (all records, positioned records): base {:?} / DLC {:?}", count(base), count(dlc));
    assert_eq!(count(base), (414, 404));
    assert_eq!(count(dlc), (430, 420));
    assert_eq!(dlc.find(crc).unwrap().index, id);
    assert_eq!(cells::resolve_entry(&mounts, crc).unwrap().0.rel, DLC_MEGAPACK);
    let (placements, counts) = cells::scan_cells(&mounts).unwrap();
    let effective: Vec<_> = placements.iter().filter(|placement| placement.cell == id).collect();
    assert_eq!(effective.len(), count(dlc).1);
    assert!(effective.iter().all(|placement| placement.pack == DLC_MEGAPACK));
    assert!(!placements.iter().any(|placement| placement.cell == 0));
    eprintln!("cell {id}: effective {} positioned placements; {} positioned placements / {} total records across effective cells", effective.len(), placements.len(), counts.values().sum::<usize>());
}
