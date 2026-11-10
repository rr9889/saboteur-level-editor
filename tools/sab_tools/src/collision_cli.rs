use std::path::Path;

pub const USAGE: &str = "sab_tools collision inspect <cell_id> [--geometry]\n  collision plan <cell_id> <hash_hex> <component> <dx> <dy> <dz>\n  collision dry-run <request.json> <new_output_directory>\n  collision module-dry-run <request.json> <new_output_directory>\n  collision module-repair <repair.json> <replacement.bin> <compiler_receipt.json> <new_output_directory>\nOffline diagnostic only; no game installation or live physics verification.";

fn cell_id(text: &str) -> Result<u32, String> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).map_err(|error| error.to_string())
    } else {
        text.parse::<u32>().map_err(|error| error.to_string())
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    let game =
        sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR")))?;
    if args.first().map(String::as_str) == Some("module-repair") && args.len() == 5 {
        let read_json = |path: &str| -> Result<serde_json::Value, String> {
            let data = std::fs::read(path).map_err(|e| e.to_string())?;
            if data.len() as u64 > sab_tools::collision_stage::MAX_REQUEST_BYTES {
                return Err("repair JSON exceeds 1 MiB".into());
            }
            serde_json::from_slice(&data).map_err(|e| e.to_string())
        };
        let repair = read_json(&args[1])?;
        let receipt = read_json(&args[3])?;
        let receipt_bytes = std::fs::read(&args[3]).map_err(|e| e.to_string())?;
        if repair["compiler_receipt_sha256"].as_str()
            != Some(sab_tools::stage::sha256(&receipt_bytes).as_str())
        {
            return Err("compiler receipt file SHA-256 mismatch".into());
        }
        let metadata = std::fs::metadata(&args[2]).map_err(|e| e.to_string())?;
        if metadata.len() > sab_formats::collision::MAX_PAYLOAD_BYTES as u64 {
            return Err("replacement exceeds collision budget".into());
        }
        let replacement = std::fs::read(&args[2]).map_err(|e| e.to_string())?;
        let manifest = sab_tools::collision_module_stage::stage_repaired(
            &game,
            &repair,
            &replacement,
            &receipt,
            Path::new(&args[4]),
        )?;
        println!(
            "{}",
            serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("plan") && args.len() == 7 {
        let hash = u32::from_str_radix(args[2].trim_start_matches("0x"), 16)
            .map_err(|error| error.to_string())?;
        let component = args[3]
            .parse::<usize>()
            .map_err(|error| error.to_string())?;
        let mut delta = [0.0; 3];
        for axis in 0..3 {
            delta[axis] = args[axis + 4]
                .parse::<f32>()
                .map_err(|error| error.to_string())?;
        }
        let plan =
            sab_tools::collision_stage::plan(&game, cell_id(&args[1])?, hash, component, delta)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&plan).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if matches!(
        args.first().map(String::as_str),
        Some("dry-run" | "module-dry-run")
    ) && args.len() == 3
    {
        let file = std::fs::File::open(&args[1]).map_err(|error| error.to_string())?;
        if file.metadata().map_err(|error| error.to_string())?.len()
            > sab_tools::collision_stage::MAX_REQUEST_BYTES
        {
            return Err("collision request exceeds 1 MiB".into());
        }
        let request = serde_json::from_reader(file).map_err(|error| error.to_string())?;
        let manifest = if args[0] == "module-dry-run" {
            sab_tools::collision_module_stage::stage_diagnostic(
                &game,
                &request,
                Path::new(&args[2]),
            )?
        } else {
            sab_tools::collision_stage::stage_dry_run(&game, &request, Path::new(&args[2]))?
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if args.first().map(String::as_str) != Some("inspect")
        || !(2..=3).contains(&args.len())
        || (args.len() == 3 && args[2] != "--geometry")
    {
        return Err(USAGE.into());
    }
    let cell = cell_id(&args[1])?;
    let inspected = sab_tools::collision::inspect_cell(&game, cell)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&sab_tools::collision::report(&inspected, args.len() == 3))
            .map_err(|error| error.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_ids_accept_explicit_radix_and_refuse_ambiguous_input() {
        assert_eq!(cell_id("1281"), Ok(1281));
        assert_eq!(cell_id("0x501"), Ok(1281));
        assert!(cell_id("-1").is_err());
        assert!(cell_id("4294967296").is_err());
        assert!(cell_id("501a").is_err());
    }
}
