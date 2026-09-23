//! `pokebot plan`: readiness planning from the command line.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use pokebot_gamedata::GameData;
use pokebot_planner::{
    battle_vs_trainer, plan_preparation, Area, Combatant, PartyMember, PlanStep, Request,
};
use pokebot_world::World;

/// Minutes per map crossed (walking plus doors/transitions).
const MINUTES_PER_MAP: f64 = 0.6;

#[derive(Debug, clap::Args)]
pub struct PlanArgs {
    /// Trainer(s) to beat, e.g. TRAINER_LEADER_BROCK (repeatable)
    #[arg(long = "against", required = true)]
    pub targets: Vec<String>,
    /// Party member as SPECIES:LEVEL[:MOVE,MOVE...], e.g. BULBASAUR:6:TACKLE,GROWL (repeatable, lead first)
    #[arg(long = "party", required = true)]
    pub party: Vec<String>,
    /// Maps reachable now for training/catching
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "Route1,Route22,Route2,ViridianForest"
    )]
    pub areas: Vec<String>,
    /// Where the player is now (for travel estimates)
    #[arg(long, default_value = "ViridianCity")]
    pub from: String,
    /// Money available (for Poké Balls)
    #[arg(long, default_value_t = 3000)]
    pub money: u32,
    /// Required win probability per battle
    #[arg(long, default_value_t = 0.9)]
    pub confidence: f64,
    #[arg(long, default_value_t = 4)]
    pub alternatives: usize,
    #[arg(long, default_value = "data/world")]
    pub world: PathBuf,
}

fn constant(prefix: &str, name: &str) -> String {
    let upper = name.trim().to_ascii_uppercase().replace([' ', '-'], "_");
    if upper.starts_with(prefix) {
        upper
    } else {
        format!("{prefix}{upper}")
    }
}

fn parse_member(spec: &str) -> Result<PartyMember> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (Some(species), Some(level)) = (parts.first(), parts.get(1)) else {
        bail!("party member {spec:?}: expected SPECIES:LEVEL[:MOVES]");
    };
    Ok(PartyMember {
        species: constant("SPECIES_", species),
        level: level
            .parse()
            .with_context(|| format!("level in {spec:?}"))?,
        exp: None,
        moves: parts.get(2).map_or(Vec::new(), |m| {
            m.split(',').map(|x| constant("MOVE_", x)).collect()
        }),
    })
}

/// Maps crossed from `from` to every map (breadth-first over warps and connections).
fn hops(world: &World, from: &str) -> HashMap<String, u32> {
    let mut dist = HashMap::from([(from.to_owned(), 0)]);
    let mut queue = VecDeque::from([from.to_owned()]);
    while let Some(name) = queue.pop_front() {
        let Some(map) = world.map(&name) else {
            continue;
        };
        let d = dist[&name];
        let next: HashSet<String> = map
            .warps
            .iter()
            .filter_map(|w| world.name_of(&w.dest_map))
            .chain(map.connections.iter().filter_map(|c| world.name_of(&c.map)))
            .map(str::to_owned)
            .collect();
        for n in next {
            dist.entry(n.clone()).or_insert_with(|| {
                queue.push_back(n);
                d + 1
            });
        }
    }
    dist
}

pub fn run(args: PlanArgs) -> Result<()> {
    let data = GameData::load(args.world.join("gamedata.json"))
        .context("run tools/gamedata/extract_gamedata.py")?;
    let world = World::load(&args.world)?;
    let party: Vec<PartyMember> = args
        .party
        .iter()
        .map(|p| parse_member(p))
        .collect::<Result<_>>()?;
    let targets: Vec<String> = args
        .targets
        .iter()
        .map(|t| constant("TRAINER_", t))
        .collect();
    for t in &targets {
        if !data.trainers.contains_key(t) {
            bail!("unknown trainer {t}");
        }
    }
    let from_here = hops(&world, &args.from);
    let areas: Vec<Area> = args
        .areas
        .iter()
        .map(|map| {
            let around = hops(&world, map);
            let center = around
                .iter()
                .filter(|(m, _)| m.contains("PokemonCenter"))
                .map(|(_, d)| *d)
                .min()
                .unwrap_or(4);
            Area {
                map: map.clone(),
                travel_minutes: f64::from(*from_here.get(map).unwrap_or(&6)) * MINUTES_PER_MAP,
                heal_minutes: 2.0 * f64::from(center) * MINUTES_PER_MAP + 0.5,
            }
        })
        .collect();

    // Where we stand today.
    let today: Vec<Combatant> = party
        .iter()
        .filter_map(|m| {
            let moves = if m.moves.is_empty() {
                data.default_moves(&m.species, m.level)
            } else {
                m.moves.clone()
            };
            Combatant::new(&data, &m.species, m.level, moves, 10)
        })
        .collect();
    println!("Current party:");
    for c in &today {
        println!(
            "  {} Lv{} {:?} (HP {})",
            c.species,
            c.level,
            c.moves,
            c.max_hp()
        );
    }
    for t in &targets {
        if let Some(e) = battle_vs_trainer(&data, &today, t) {
            println!("Today vs {}: P(win) = {:.1}%", e.trainer, e.p_win * 100.0);
            for (ours, theirs, m) in &e.matchups {
                println!(
                    "    {ours} vs {theirs}: {:.1}% (ours {:?} / theirs {:?}, ~{:.1} turns)",
                    m.p_win * 100.0,
                    m.our_move,
                    m.their_move,
                    m.turns
                );
            }
        }
    }

    let request = Request {
        party,
        targets,
        areas,
        confidence: args.confidence,
        money: args.money,
        data: &data,
    };
    let plans = plan_preparation(&request, args.alternatives);
    println!(
        "\nPlans (target ≥ {:.0}% per battle), cheapest first:",
        args.confidence * 100.0
    );
    for (i, plan) in plans.iter().enumerate() {
        println!(
            "{}. ~{:.1} min, confidence {}",
            i + 1,
            plan.minutes,
            plan.confidence
                .iter()
                .map(|(t, p)| format!("{t} {:.1}%", p * 100.0))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for step in &plan.steps {
            match step {
                PlanStep::Catch {
                    species,
                    map,
                    level,
                    minutes,
                    balls,
                } => {
                    println!("     catch {species} (Lv~{level}) on {map}: ~{minutes:.1} min, ~{balls} Poké Balls")
                }
                PlanStep::Train {
                    species,
                    from,
                    to,
                    map,
                    minutes,
                    battles,
                } => {
                    println!("     train {species} Lv{from} → Lv{to} on {map}: ~{battles} battles, ~{minutes:.1} min")
                }
            }
        }
        for (species, level, moves) in &plan.party {
            println!("       → {species} Lv{level} {moves:?}");
        }
    }
    Ok(())
}
