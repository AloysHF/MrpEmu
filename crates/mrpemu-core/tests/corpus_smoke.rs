//! Corpus smoke gate: samples the reference corpus (tmp/mrp_game), boots
//! each package headlessly, and classifies the outcome. Ignored by default
//! (`-- --ignored`) because it needs the local corpus; CI runs it nightly.

use mrpemu_core::emulator::MrpMachine;

/// How many packages one survey pass samples.
const SAMPLE_SIZE: usize = 200;
/// Frames per package (plus one Ok press mid-run).
const FRAMES_PER_SAMPLE: u32 = 90;

fn collect_mrps(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("mrp") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn classify(machine: &MrpMachine, pixels_nonzero: usize, total: usize) -> &'static str {
    if machine.boot_error().is_some() {
        return "boot-failed";
    }
    let ratio = pixels_nonzero as f64 / total.max(1) as f64;
    if pixels_nonzero == 0 {
        "blank"
    } else if ratio < 0.005 {
        "sparse"
    } else {
        "content"
    }
}

/// Samples the corpus and prints one classification line per package plus a
/// summary tally. `CORPUS_SAMPLE` overrides the deterministic sample.
#[test]
#[ignore = "requires the local reference corpus under tmp/mrp_game"]
fn corpus_smoke_sample() {
    let _ = env_logger::try_init();
    let root = std::path::Path::new("../../tmp/mrp_game");
    let all = collect_mrps(root);
    assert!(
        all.len() > 1000,
        "corpus should hold thousands of packages, found {}",
        all.len()
    );

    // Deterministic LCG scatter-sample: stride the sorted list with an
    // odd multiplier so every region contributes without a rand dep.
    let mut picks: Vec<std::path::PathBuf> = Vec::with_capacity(SAMPLE_SIZE.min(all.len()));
    let stride = (all.len() as u64 / SAMPLE_SIZE.max(1) as u64).max(1) | 1;
    let mut cursor = (stride * 7) % all.len() as u64;
    for _ in 0..SAMPLE_SIZE.min(all.len()) {
        picks.push(all[cursor as usize].clone());
        cursor = (cursor + stride) % all.len() as u64;
    }
    picks.sort();

    let mut tally: std::collections::BTreeMap<&'static str, Vec<String>> =
        std::collections::BTreeMap::new();
    // CORPUS_ONLY=<substring> restricts the run to matching packages.
    let only = std::env::var("CORPUS_ONLY").ok();
    for path in &picks {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        if let Some(only) = &only {
            if !name.contains(only.as_str()) {
                continue;
            }
        }
        // A guest panicking inside the ARM core must not abort the whole
        // sweep; classify it and keep walking the corpus.
        let outcome = std::panic::catch_unwind(|| -> &'static str {
            let Ok(mut machine) = MrpMachine::from_path(path) else {
                return "load-failed";
            };
            for frame in 0..FRAMES_PER_SAMPLE {
                machine.run_frame();
                if frame == 40 {
                    machine.input_mut().press(mrpemu_core::MrpKey::Ok);
                }
                if frame == 44 {
                    machine.input_mut().release(mrpemu_core::MrpKey::Ok);
                }
            }
            let screen = machine.screen();
            let nonzero = screen.pixels().iter().filter(|&&p| p != 0).count();
            let total = screen.pixels().len();
            classify(&machine, nonzero, total)
        })
        .unwrap_or("crash");
        eprintln!("{outcome:12} {name}");
        tally.entry(outcome).or_default().push(name);
    }

    eprintln!("=== corpus smoke summary ({} sampled) ===", picks.len());
    for (outcome, names) in &tally {
        eprintln!("{outcome:12} {:4}  e.g. {}", names.len(), names[0]);
    }
    let _ = std::env::var("CORPUS_ONLY");
}
