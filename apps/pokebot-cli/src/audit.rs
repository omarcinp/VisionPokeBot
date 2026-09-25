//! `pokebot audit`: replays recorded sessions through perception and reports
//! what it recognised, frame by frame, so screens it misses can be found
//! from data. Unrecognised frames are grouped by a coarse colour layout and
//! one exemplar PNG per group is written, largest groups first.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use pokebot_core::{NormalizedFrame, RgbImage};
use pokebot_replay::FrameRecord;
use pokebot_state::{Observation, StateReducer};
use pokebot_vision::{FireRedPerception, PerceptionSystem};

#[derive(Debug, clap::Args)]
pub struct AuditArgs {
    /// Recorded session directories (frames.jsonl + frames/)
    #[arg(required = true)]
    sessions: Vec<PathBuf>,
    /// Where the report and exemplar frames go
    #[arg(long, default_value = "captures/audit")]
    out: PathBuf,
    /// World model and fonts (see tools/world/build.sh)
    #[arg(long, default_value = "data/world")]
    world: PathBuf,
    /// Locate the player on the world model (slow without hints); every
    /// located frame's pose goes to `poses.jsonl`
    #[arg(long)]
    locate: bool,
    /// Read every Nth recorded frame
    #[arg(long, default_value_t = 1)]
    every: usize,
    /// Exemplars written per group
    #[arg(long, default_value_t = 3)]
    exemplars: usize,
    /// Also run the sensor, reducer and diff, as the runtime does, and
    /// write every state change to `changes.jsonl`
    #[arg(long)]
    changes: bool,
    /// Start from this `state.json` (the knowledge saved with the game):
    /// the sensor places what it reads in the party by name
    #[arg(long)]
    state: Option<PathBuf>,
}

/// The runtime's state pipeline, without devices: extractor and sensor
/// events into the reducer, the changes out.
struct Pipeline {
    extractor: pokebot_state::EventExtractor,
    sensor: pokebot_sense::Sensor,
    state: pokebot_state::GameState,
    out: std::io::BufWriter<std::fs::File>,
    counts: BTreeMap<String, usize>,
    epoch: Instant,
}

impl Pipeline {
    fn new(args: &AuditArgs) -> Result<Self> {
        let data = pokebot_gamedata::GameData::load(args.world.join("gamedata.json"))?;
        let mut state = pokebot_state::GameState::default();
        if let Some(path) = &args.state {
            let saved: pokebot_state::SavedKnowledge = serde_json::from_str(
                &std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?,
            )?;
            state = pokebot_state::DefaultReducer.reduce(
                &state,
                &[pokebot_state::EventRecord {
                    frame_id: 0,
                    event: pokebot_state::GameEvent::CheckpointRestored {
                        knowledge: Box::new(saved),
                    },
                }],
            );
        }
        Ok(Self {
            extractor: pokebot_state::EventExtractor::default(),
            sensor: pokebot_sense::Sensor::new(Arc::new(data)),
            state,
            out: std::io::BufWriter::new(std::fs::File::create(args.out.join("changes.jsonl"))?),
            counts: BTreeMap::new(),
            epoch: Instant::now(),
        })
    }

    fn observe(&mut self, o: &Observation, record: &FrameRecord) -> Result<()> {
        use std::io::Write;
        let arrival = pokebot_state::FrameArrival {
            frame_id: record.frame_id,
            delivered: record.delivered.unwrap_or(record.frame_id),
            captured_at: self.epoch + std::time::Duration::from_micros(record.elapsed_us),
        };
        let mut events = self.extractor.observe(o, arrival);
        events.extend(
            self.sensor
                .observe(o, &self.state)
                .into_iter()
                .map(|event| pokebot_state::EventRecord {
                    frame_id: o.frame_id,
                    event,
                }),
        );
        if events.is_empty() {
            return Ok(());
        }
        let next = pokebot_state::DefaultReducer.reduce(&self.state, &events);
        for change in pokebot_state::diff(&self.state, &next) {
            let json = serde_json::to_value(&change)?;
            let name = json
                .as_object()
                .and_then(|m| m.keys().next().cloned())
                .or_else(|| json.as_str().map(str::to_owned))
                .unwrap_or_default();
            *self.counts.entry(name).or_default() += 1;
            writeln!(
                self.out,
                "{}",
                serde_json::json!({"frame_id": o.frame_id, "change": json})
            )?;
        }
        self.state = next;
        Ok(())
    }
}

/// 12×8 cells of 20×20 px, each its mean colour at 2 bits per channel.
const CELLS_X: u32 = 12;
const CELLS_Y: u32 = 8;
/// Cells that may differ within one group.
const GROUP_DISTANCE: usize = 10;

type Layout = Vec<u8>;

fn layout(image: &RgbImage) -> Layout {
    let (cw, ch) = (image.width() / CELLS_X, image.height() / CELLS_Y);
    let mut out = Vec::with_capacity((CELLS_X * CELLS_Y) as usize);
    for cy in 0..CELLS_Y {
        for cx in 0..CELLS_X {
            let mut sum = [0u32; 3];
            for y in (cy * ch..(cy + 1) * ch).step_by(2) {
                for x in (cx * cw..(cx + 1) * cw).step_by(2) {
                    let p = image.pixel(x, y);
                    for c in 0..3 {
                        sum[c] += u32::from(p[c]);
                    }
                }
            }
            let n = (cw / 2) * (ch / 2);
            let q = |v: u32| ((v / n.max(1)) >> 6) as u8;
            out.push(q(sum[0]) << 4 | q(sum[1]) << 2 | q(sum[2]));
        }
    }
    out
}

impl Group {
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind, "count": self.count,
            "examples": self.examples, "files": self.files,
        })
    }
}

fn distance(a: &Layout, b: &Layout) -> usize {
    a.iter().zip(b).filter(|(x, y)| x != y).count()
}

#[derive(Debug)]
struct Group {
    /// What perception said about the frames (screen/detector).
    kind: String,
    count: usize,
    /// (session, frame id) of the first frames seen.
    examples: Vec<(String, u64)>,
    files: Vec<String>,
    layout: Layout,
}

/// What an observation recognised, as `screen/detector` plus the payloads
/// it carries; frames that recognise nothing are `Unknown/none`.
fn kind(o: &Observation) -> String {
    let mut parts = vec![format!("{:?}/{}", o.screen.value, o.screen.detector)];
    let mut has = |present: bool, name: &str| {
        if present {
            parts.push(name.to_owned());
        }
    };
    has(o.player.is_some(), "+pose");
    has(o.battle.is_some(), "+battle");
    has(o.menu.is_some(), "+menu");
    has(o.dialogue.is_some(), "+dialogue");
    has(o.map_popup.is_some(), "+popup");
    parts.join(" ")
}

/// Readings that came back partial: text with unknown glyphs, a battle HUD
/// whose bars are visible but whose name or HP numbers weren't read.
fn partial(o: &Observation) -> Vec<&'static str> {
    let mut out = Vec::new();
    if let Some(d) = &o.dialogue {
        if d.stable_frames > 0 && d.lines.iter().any(|l| l.contains('?')) {
            out.push("dialogue text with unknown glyphs");
        }
        if d.stable_frames > 0 && d.lines.iter().all(|l| l.trim().is_empty()) {
            out.push("dialogue box with no text read");
        }
    }
    if let Some(b) = &o.battle {
        if b.player_hp.is_some() && b.player_name.is_none() {
            out.push("battle: player bar without name");
        }
        if b.player_hp.is_some() && b.player_hp_numbers.is_none() {
            out.push("battle: player bar without HP numbers");
        }
        if b.opponent_hp.is_some() && b.opponent_name.is_none() {
            out.push("battle: opponent bar without name");
        }
        if b.player_name.as_deref().is_some_and(|n| n.contains('?')) {
            out.push("battle: player name with unknown glyphs");
        }
        if b.opponent_name.as_deref().is_some_and(|n| n.contains('?')) {
            out.push("battle: opponent name with unknown glyphs");
        }
    }
    if !o.menu_lines.is_empty() && o.menu_lines.iter().any(|l| l.contains('?')) {
        out.push("menu rows with unknown glyphs");
    }
    out
}

fn read_frames(dir: &Path) -> Result<Vec<FrameRecord>> {
    let path = dir.join("frames.jsonl");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    // A live recording's last line may still be being written.
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

fn perception(world: &Path, locate: bool) -> Result<FireRedPerception> {
    let mut p = if locate {
        FireRedPerception::with_world(Arc::new(pokebot_world::World::load(world)?))
            .with_global_search(true)
    } else {
        FireRedPerception::default()
    };
    let font = world.join("font_normal.json");
    if font.exists() {
        p = p.with_font(Arc::new(pokebot_vision::text::Font::load(&font)?));
    }
    let small = world.join("font_small.json");
    if small.exists() {
        p = p.with_small_font(Arc::new(pokebot_vision::text::Font::load(&small)?));
    }
    let data = world.join("gamedata.json");
    if data.exists() {
        let data = pokebot_gamedata::GameData::load(&data)?;
        p = p.with_palettes(Arc::new(crate::sprite_palettes(&data)));
    }
    Ok(p)
}

pub fn run(args: AuditArgs) -> Result<()> {
    std::fs::create_dir_all(args.out.join("frames"))?;
    let mut histogram: BTreeMap<String, usize> = BTreeMap::new();
    let mut partials: BTreeMap<&'static str, Vec<Group>> = BTreeMap::new();
    let mut unknown: Vec<Group> = Vec::new();
    let mut total = 0usize;
    // Time spent in `observe` per frame (µs): perception's cost without
    // the PNG decoding around it.
    let mut timings: Vec<u64> = Vec::new();
    let mut poses = args
        .locate
        .then(|| std::fs::File::create(args.out.join("poses.jsonl")).map(std::io::BufWriter::new))
        .transpose()?;
    let started = Instant::now();
    for dir in &args.sessions {
        let name = dir
            .file_name()
            .map_or("session".into(), |n| n.to_string_lossy().into_owned());
        let frames = read_frames(dir)?;
        eprintln!("{name}: {} recorded frames", frames.len());
        // Perception keeps state across frames (text settling, pose hint),
        // so each session gets its own.
        let mut p = perception(&args.world, args.locate)?;
        let mut pipeline = args.changes.then(|| Pipeline::new(&args)).transpose()?;
        for record in frames.iter().step_by(args.every.max(1)) {
            let Ok(image) = pokebot_video::png::load(dir.join(&record.file)) else {
                continue;
            };
            let frame = NormalizedFrame::new(record.frame_id, Instant::now(), image)?;
            let observed = Instant::now();
            let o = p.observe(&frame);
            timings.push(observed.elapsed().as_micros() as u64);
            if let (Some(out), Some(p)) = (&mut poses, &o.player) {
                use std::io::Write;
                let line = serde_json::json!({
                    "session": name, "frame_id": record.frame_id, "file": record.file,
                    "map": p.pose.map, "x": p.pose.x, "y": p.pose.y, "score": p.score,
                });
                writeln!(out, "{line}")?;
            }
            if let Some(pipeline) = &mut pipeline {
                pipeline.observe(&o, record)?;
            }
            total += 1;
            let k = kind(&o);
            *histogram.entry(k.clone()).or_default() += 1;
            let unrecognised = o.screen.detector == "none" && o.player.is_none();
            let mut groups: Vec<&mut Vec<Group>> = Vec::new();
            if unrecognised {
                groups.push(&mut unknown);
            }
            let partial = partial(&o);
            for reason in &partial {
                partials.entry(reason).or_default();
            }
            for (reason, list) in partials.iter_mut() {
                if partial.contains(reason) {
                    groups.push(list);
                }
            }
            if groups.is_empty() {
                continue;
            }
            let l = layout(frame.image());
            for list in groups {
                add(list, &k, &l, (&name, record.frame_id), frame.image(), &args)?;
            }
            if total % 2000 == 0 {
                eprintln!("  {total} frames, {:.0?}", started.elapsed());
            }
        }
        if let Some(pipeline) = pipeline {
            println!(
                "\nState changes in {name} ({}):",
                args.out.join("changes.jsonl").display()
            );
            let mut counts: Vec<_> = pipeline.counts.into_iter().collect();
            counts.sort_by_key(|c| std::cmp::Reverse(c.1));
            for (kind, n) in counts {
                println!("  {n:>6}  {kind}");
            }
            std::fs::write(
                args.out.join(format!("{name}-final-state.json")),
                serde_json::to_string_pretty(&pipeline.state)?,
            )?;
        }
    }
    unknown.sort_by_key(|g| std::cmp::Reverse(g.count));
    for list in partials.values_mut() {
        list.sort_by_key(|g| std::cmp::Reverse(g.count));
    }
    let mut histogram: Vec<(String, usize)> = histogram.into_iter().collect();
    histogram.sort_by_key(|h| std::cmp::Reverse(h.1));
    let report = serde_json::json!({
        "sessions": args.sessions,
        "frames": total,
        "histogram": histogram,
        "unknown_groups": unknown.iter().map(Group::json).collect::<Vec<_>>(),
        "partial_readings": partials
            .iter()
            .map(|(k, v)| (*k, v.iter().map(Group::json).collect::<Vec<_>>()))
            .collect::<BTreeMap<_, _>>(),
    });
    std::fs::write(
        args.out.join("report.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    println!("{total} frames in {:.0?}", started.elapsed());
    timings.sort_unstable();
    if let Some(&max) = timings.last() {
        let at = |q: usize| timings[(timings.len() - 1) * q / 100] as f64 / 1000.0;
        let mean = timings.iter().sum::<u64>() as f64 / timings.len() as f64 / 1000.0;
        println!(
            "perception: mean {mean:.2} ms/frame, p50 {:.2}, p90 {:.2}, p99 {:.2}, max {:.1} ms",
            at(50),
            at(90),
            at(99),
            max as f64 / 1000.0
        );
    }
    println!("\nWhat perception recognised:");
    for (k, n) in &histogram {
        println!("  {n:>7}  {:>5.1}%  {k}", *n as f64 * 100.0 / total as f64);
    }
    let unknown_frames: usize = unknown.iter().map(|g| g.count).sum();
    println!(
        "\nUnrecognised: {unknown_frames} frames in {} groups (largest first):",
        unknown.len()
    );
    for g in unknown.iter().take(40) {
        println!("  {:>6}  {}", g.count, g.files.join(" "));
    }
    for (reason, list) in &partials {
        let n: usize = list.iter().map(|g| g.count).sum();
        println!("\nPartial: {reason}: {n} frames in {} groups", list.len());
        for g in list.iter().take(10) {
            println!("  {:>6}  {}", g.count, g.files.join(" "));
        }
    }
    println!("\nreport: {}", args.out.join("report.json").display());
    Ok(())
}

fn add(
    list: &mut Vec<Group>,
    kind: &str,
    layout: &Layout,
    (session, frame_id): (&str, u64),
    image: &RgbImage,
    args: &AuditArgs,
) -> Result<()> {
    let at = list
        .iter()
        .position(|g| g.kind == kind && distance(&g.layout, layout) <= GROUP_DISTANCE);
    let index = match at {
        Some(i) => i,
        None => {
            list.push(Group {
                kind: kind.to_owned(),
                count: 0,
                examples: Vec::new(),
                files: Vec::new(),
                layout: layout.clone(),
            });
            list.len() - 1
        }
    };
    let group = &mut list[index];
    group.count += 1;
    // Exemplars spread over the group: the first, then one per 50 frames.
    if group.files.len() < args.exemplars && (group.count == 1 || group.count % 50 == 0) {
        let file = format!("frames/{session}-{frame_id}.png");
        let path = args.out.join(&file);
        if !path.exists() {
            pokebot_video::png::save(image, &path)?;
        }
        group.examples.push((session.to_owned(), frame_id));
        group.files.push(file);
    }
    Ok(())
}
