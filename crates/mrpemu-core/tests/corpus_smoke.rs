//! Corpus smoke gate: samples the reference corpus (tmp/mrp_game), boots
//! each package headlessly, and classifies the outcome. Ignored by default
//! (`-- --ignored`) because it needs the local corpus; CI runs it nightly.

use mrpemu_core::emulator::{MrpMachine, FRAME_RATE};
use mrpemu_core::MrpKey;

use std::collections::BTreeMap;
use std::time::Duration;

/// How many packages one survey pass samples.
const SAMPLE_SIZE: usize = 200;
/// Override the corpus root to test only the hot games.
const HOT_GAMES_ROOT: &str = "../../tmp/mrp_game/热门游戏";
/// Frames per package with a sequence of confirm and soft-key presses.
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

fn frame_coverage(pixels: &[u16], width: usize) -> u8 {
    if pixels.iter().all(|&pixel| pixel == pixels[0]) {
        return 0;
    }
    let nonzero = pixels.iter().filter(|&&pixel| pixel != 0).count();
    let rows = pixels
        .chunks_exact(width)
        .filter(|row| row.iter().any(|&pixel| pixel != 0))
        .count();
    let height = pixels.len() / width;
    if nonzero * 200 >= pixels.len() && rows * 8 >= height {
        2
    } else {
        1
    }
}

#[test]
fn pixel_strip_does_not_count_as_substantial_video() {
    let mut pixels = vec![0; 240 * 400];
    assert_eq!(frame_coverage(&pixels, 240), 0);
    pixels[..240 * 4].fill(0xffff);
    assert_eq!(frame_coverage(&pixels, 240), 1);
    pixels[..240 * 100].fill(0xffff);
    assert_eq!(frame_coverage(&pixels, 240), 2);
}

/// Samples the corpus and prints one classification line per package plus a
/// summary tally with resolution and miss-classification breakdowns.
#[test]
#[ignore = "requires the local reference corpus under tmp/mrp_game"]
fn corpus_smoke_sample() {
    let _ = env_logger::try_init();
    // Total instruction budget per package (script + guest): a stuck loop
    // exhausts this and classifies fast instead of burning minutes.
    mrpemu_core::cfunction::set_instruction_budget(512_000_000);
    // CORPUS_HOT=1 restricts to the 热门游戏 directory.
    let (_root, all) = if std::env::var("CORPUS_HOT").is_ok() {
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
    assert!(!all.is_empty(), "no packages found in {}", _root.display());
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
    let mut tested = 0;
    let system_dir = std::env::var_os("CORPUS_SYSTEM_DIR").map(std::path::PathBuf::from);
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
        tested += 1;
        let (outcome, misses, res_key) = std::panic::catch_unwind(|| {
            mrpemu_core::cfunction::reset_instruction_counter();
            let Ok(mut machine) = MrpMachine::from_path(path) else {
                return ("load-failed", Vec::new(), String::new());
            };
            if let Some(directory) = &system_dir {
                for &name in mrpemu_core::vfs::DEVICE_FILES {
                    if let Ok(data) = std::fs::read(directory.join(name)) {
                        machine.vfs_mut().write(name, data);
                    }
                }
            }
            let res_key = format!("{}x{}", machine.screen().width(), machine.screen().height());
            let mut best_coverage = 0;
            for frame in 0..FRAMES_PER_SAMPLE {
                for (press_frame, key) in [
                    (30u32, MrpKey::LeftSoft),
                    (50, MrpKey::Ok),
                    (85, MrpKey::LeftSoft),
                    (105, MrpKey::Ok),
                    (140, MrpKey::LeftSoft),
                    (160, MrpKey::Ok),
                ] {
                    if frame == press_frame {
                        machine.input_mut().press(key);
                    }
                    if frame == press_frame + 4 {
                        machine.input_mut().release(key);
                    }
                }
                machine.run_frame_at(Duration::from_secs_f64(f64::from(frame) / FRAME_RATE));
                if machine.boot_error().is_some() {
                    break;
                }
                if best_coverage < 2 {
                    let screen = machine.screen();
                    best_coverage = best_coverage
                        .max(frame_coverage(screen.pixels(), usize::from(screen.width())));
                }
            }
            let outcome = match best_coverage {
                _ if machine.boot_error().is_some() => "boot-failed",
                0 => "blank",
                1 => "sparse",
                _ => "content",
            };
            let misses = if matches!(outcome, "blank" | "boot-failed") {
                machine.missing_globals()
            } else {
                Vec::new()
            };
            if let Some(error) = machine.boot_error() {
                eprintln!("{name}: {error}");
            }
            (outcome, misses, res_key)
        })
        .unwrap_or(("crash", Vec::new(), String::new()));
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
        tested,
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
    assert!(tested > 0, "no packages matched CORPUS_ONLY");
    if std::env::var_os("CORPUS_REQUIRE_VIDEO").is_some() {
        let failures: Vec<_> = tally
            .iter()
            .filter(|(outcome, _)| **outcome != "content")
            .flat_map(|(_, names)| names)
            .collect();
        assert!(
            failures.is_empty(),
            "packages without a substantial frame: {failures:?}"
        );
    }
}
