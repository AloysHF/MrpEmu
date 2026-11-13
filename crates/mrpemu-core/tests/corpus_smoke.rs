//! Corpus smoke gate: samples the reference corpus (tmp/mrp_game), boots
//! each package headlessly, and classifies the outcome. Ignored by default
//! (`-- --ignored`) because it needs the local corpus; CI runs it nightly.

use mrpemu_core::emulator::MrpMachine;
use mrpemu_core::MrpKey;

use std::collections::BTreeMap;

/// How many packages one survey pass samples.
const SAMPLE_SIZE: usize = 200;
/// Override the corpus root to test only the hot games.
const HOT_GAMES_ROOT: &str = "../../tmp/mrp_game/热门游戏";
/// Frames per package (plus one Ok press mid-run).
const FRAMES_PER_SAMPLE: u32 = 200;

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

/// Detects the screen resolution from the corpus directory name.
fn detect_resolution(path: &std::path::Path) -> (u16, u16) {
    let text = path.to_string_lossy();
    if text.contains("240x400") || text.contains("240×400") {
        (240, 400)
    } else if text.contains("320x480") || text.contains("320×480") {
        (320, 480)
    } else {
        (240, 320)
    }
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
/// summary tally with resolution and miss-classification breakdowns.
#[test]
#[ignore = "requires the local reference corpus under tmp/mrp_game"]
fn corpus_smoke_sample() {
    let _ = env_logger::try_init();
    // Total instruction budget per package (script + guest): a stuck loop
    // exhausts this and classifies fast instead of burning minutes.
    mrpemu_core::cfunction::set_instruction_budget(20_000_000);
    // CORPUS_HOT=1 restricts to the 热门游戏 directory.
    let (root, all) = if std::env::var("CORPUS_HOT").is_ok() {
        let root = std::path::Path::new(HOT_GAMES_ROOT);
        (root, collect_mrps(root))
    } else {
        let root = std::path::Path::new("../../tmp/mrp_game");
        let all = collect_mrps(root);
        assert!(
            all.len() > 1000,
            "corpus should hold thousands of packages, found {}",
            all.len()
        );
        (root, all)
    };

    // Deterministic LCG scatter-sample.
    let sample_count = if std::env::var("CORPUS_HOT").is_ok() {
        all.len()
    } else {
        SAMPLE_SIZE.min(all.len())
    };
    let mut picks: Vec<std::path::PathBuf> = Vec::with_capacity(sample_count);
    let stride = (all.len() as u64 / sample_count.max(1) as u64).max(1) | 1;
    let mut cursor = (stride * 7) % all.len() as u64;
    for _ in 0..sample_count {
        picks.push(all[cursor as usize].clone());
        cursor = (cursor + stride) % all.len() as u64;
    }
    picks.sort();

    let mut tally: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    let mut miss_tally: BTreeMap<String, usize> = BTreeMap::new();
    let mut res_tally: BTreeMap<String, BTreeMap<&'static str, usize>> = BTreeMap::new();
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
        let (res_w, res_h) = detect_resolution(path);
        let res_key = format!("{res_w}x{res_h}");
        let (outcome, misses) = std::panic::catch_unwind(|| -> (&'static str, Vec<String>) {
            mrpemu_core::cfunction::reset_instruction_counter();
            let Ok(mut machine) = MrpMachine::from_path(path) else {
                return ("load-failed", Vec::new());
            };
            machine.set_resolution(res_w, res_h);
            for frame in 0..FRAMES_PER_SAMPLE {
                machine.run_frame();
                // richer input: dismiss splash, navigate menus, confirm
                for (press_frame, key) in [
                    (40u32, MrpKey::Ok),
                    (44, MrpKey::Ok),
                    (80, MrpKey::Ok),
                    (84, MrpKey::Ok),
                    (120, MrpKey::Ok),
                    (124, MrpKey::Ok),
                ] {
                    if frame == press_frame {
                        machine.input_mut().press(key);
                    }
                    if frame == press_frame + 4 {
                        machine.input_mut().release(key);
                    }
                }
            }
            let screen = machine.screen();
            let nonzero = screen.pixels().iter().filter(|&&p| p != 0).count();
            let total = screen.pixels().len();
            let outcome = classify(&machine, nonzero, total);
            let misses = if matches!(outcome, "blank" | "boot-failed") {
                machine.missing_globals()
            } else {
                Vec::new()
            };
            (outcome, misses)
        })
        .unwrap_or(("crash", Vec::new()));
        for miss in &misses {
            *miss_tally.entry(format!("global:{miss}")).or_default() += 1;
        }
        eprintln!("{outcome:12} [{res_key}] {name}");
        tally.entry(outcome).or_default().push(name);
        *res_tally
            .entry(res_key)
            .or_default()
            .entry(outcome)
            .or_default() += 1;
    }

    eprintln!(
        "=== corpus smoke summary ({} of {} sampled) ===",
        picks.len(),
        all.len()
    );
    for (outcome, names) in &tally {
        eprintln!("{outcome:12} {:4}  e.g. {}", names.len(), names[0]);
    }
    eprintln!("=== by resolution ===");
    for (res, outcomes) in &res_tally {
        let parts: Vec<String> = outcomes.iter().map(|(o, n)| format!("{o}:{n}")).collect();
        eprintln!("  {res}: {}", parts.join(" "));
    }
    if !miss_tally.is_empty() {
        eprintln!("=== top missing globals (blank/boot-failed) ===");
        let mut sorted: Vec<_> = miss_tally.iter().collect();
        sorted.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        for (name, count) in sorted.iter().take(15) {
            eprintln!("  {name}: {count}");
        }
    }
}
