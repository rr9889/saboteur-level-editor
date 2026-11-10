//! `sab_tools` — command-line tools over `sab_formats`.
//!
//! ```text
//! sab_tools places ...   placement editor agent interface (JSON in/out), see src/places.rs
//! sab_tools m0 survey-cells <x> <y> <z> <radius>
//! sab_tools m0 survey-editnodes [name_substring | 0xCONTAINER]
//! sab_tools m0 stage-e1 <0xCONTAINER> <0xTAG> <dx> <dy> <dz> <out_dir>
//! sab_tools m0 stage-e1b <out_dir>          (Gate A: rotate, duplicate, delete)
//! sab_tools m0 stage-e2 <cell_id> <0xTAG> <dx> <dy> <dz> <out_dir>
//! ```
//!
//! The game install comes from `SAB_RETAIL_DIR` or `retail_dir` in `.sab-local.toml`. Nothing here
//! writes into the game directory: staging writes artifacts plus install/restore scripts to an output
//! folder, and the user runs the scripts.

mod m0;
mod places;
mod collision_cli;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("m0") => m0::run(&args[1..]),
        Some("places") => places::run(&args[1..]),
        Some("collision") => collision_cli::run(&args[1..]),
        Some("village") => village_cli(&args[1..]),
        _ => Err(format!("{USAGE}\n{}\n{}", places::USAGE, collision_cli::USAGE)),
    };
    if let Err(e) = r {
        if !e.is_empty() {
            eprintln!("{e}");
        }
        std::process::exit(1);
    }
}

const USAGE: &str = "usage:
  sab_tools m0 survey-cells <x> <y> <z> <radius>
  sab_tools m0 survey-editnodes [name_substring | 0xCONTAINER]
  sab_tools m0 stage-e1 <0xCONTAINER> <0xTAG> <dx> <dy> <dz> <out_dir>
  sab_tools m0 stage-e1b <out_dir>
  sab_tools m0 stage-e2 <cell_id> <0xTAG> <dx> <dy> <dz> <out_dir>";

/// `sab_tools village author <spec.json> <project.json>` | `sab_tools village export <project.json> <out_dir> [<record>=<payload.bin> ...]`
fn village_cli(args: &[String]) -> Result<(), String> {
    let game = || sab_formats::game_paths::local_config_retail_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    match args.first().map(String::as_str) {
        Some("author") if args.len() == 3 => {
            let spec: serde_json::Value = serde_json::from_slice(&std::fs::read(&args[1]).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let r = sab_tools::village::author(&game()?, &spec, std::path::Path::new(&args[2]))?;
            println!("{}", serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?);
            Ok(())
        }
        Some("export") if args.len() >= 3 => {
            let mut recs = Vec::new();
            for a in &args[3..] {
                let (r, p) = a.split_once('=').ok_or("record replacement must be <record>=<payload.bin>")?;
                recs.push((r.parse::<usize>().map_err(|e| e.to_string())?, std::fs::read(p).map_err(|e| format!("{p}: {e}"))?));
            }
            let r = sab_tools::village::export(std::path::Path::new(&args[1]), &recs, std::path::Path::new(&args[2]))?;
            println!("{}", serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?);
            Ok(())
        }
        _ => Err("usage: sab_tools village author <spec.json> <project.json> | village export <project.json> <out_dir> [<record>=<payload.bin> ...]".into()),
    }
}
