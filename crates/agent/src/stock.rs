//! How many Poké Balls to keep and buy.
//!
//! The bot keeps a reserve of [`SHINY_RESERVE`] balls that only a shiny may
//! use, restocks toward [`TARGET_STOCK`] whenever it can, and never spends
//! the money needed for [`POTIONS_KEPT`] Potions. An unknown pocket gives
//! a `None` count: the caller audits before relying on it.

use pokebot_gamedata::mechanics::ball_multiplier;
use pokebot_gamedata::GameData;
use pokebot_state::GameState;

use crate::catch::balls_held;

/// Balls only a shiny may use.
pub const SHINY_RESERVE: u16 = 5;
/// Stock a mart visit restocks to.
pub const TARGET_STOCK: u16 = 15;
/// Potions whose price is never spent on balls.
pub const POTIONS_KEPT: u32 = 2;

/// All balls in the Poké Balls pocket (the Master Ball excluded); `None`
/// when the pocket is unknown. A tracked (stale) list counts as known.
pub fn ball_count(state: &GameState) -> Option<u16> {
    let balls = balls_held(state)?;
    Some(
        balls
            .iter()
            .filter(|(item, _)| ball_multiplier(item).is_some())
            .fold(0u16, |sum, (_, n)| sum.saturating_add(*n)),
    )
}

/// Poké Balls to buy to reach [`TARGET_STOCK`], keeping the price of
/// [`POTIONS_KEPT`] Potions. 0 when either price is missing from the data.
pub fn buy_count(data: &GameData, stock: u16, money: u32) -> u16 {
    let price = |item: &str| data.items.get(item).map(|i| i.price).filter(|p| *p > 0);
    let (Some(potion), Some(ball)) = (price("ITEM_POTION"), price("ITEM_POKE_BALL")) else {
        return 0;
    };
    let need = TARGET_STOCK.saturating_sub(stock);
    let budget = money.saturating_sub(POTIONS_KEPT * potion);
    let affordable = budget / ball;
    need.min(u16::try_from(affordable).unwrap_or(u16::MAX))
}

/// The stock is known to be below the shiny reserve: buy before going on.
pub fn must_buy(stock: Option<u16>) -> bool {
    stock.is_some_and(|n| n < SHINY_RESERVE)
}

/// Worth buying at a mart: stock unknown or below the target.
pub fn should_buy(stock: Option<u16>) -> bool {
    stock.is_none_or(|n| n < TARGET_STOCK)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pokebot_gamedata::GameData;
    use pokebot_state::{DefaultReducer, EventRecord, GameEvent, Pocket, StateReducer};

    use super::*;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    #[test]
    fn buy_count_is_capped_by_money_minus_two_potions() {
        let Some(data) = data() else { return };
        // (1000 − 2 × 300) / 200 = 2.
        assert_eq!(buy_count(&data, 3, 1000), 2);
    }

    #[test]
    fn buy_count_reaches_the_target() {
        let Some(data) = data() else { return };
        assert_eq!(buy_count(&data, 3, 99_999), 12);
        assert_eq!(buy_count(&data, TARGET_STOCK, 99_999), 0);
        assert_eq!(buy_count(&data, 20, 99_999), 0);
    }

    #[test]
    fn no_money_buys_nothing() {
        let Some(data) = data() else { return };
        assert_eq!(buy_count(&data, 3, 500), 0);
        assert_eq!(buy_count(&data, 3, 0), 0);
        // Unknown prices: buy nothing rather than drop the Potion reserve.
        let mut no_potion = data;
        no_potion.items.remove("ITEM_POTION");
        assert_eq!(buy_count(&no_potion, 3, 99_999), 0);
        let Some(mut no_ball) = self::data() else {
            return;
        };
        no_ball.items.remove("ITEM_POKE_BALL");
        assert_eq!(buy_count(&no_ball, 3, 99_999), 0);
    }

    #[test]
    fn ball_count_skips_the_master_ball_and_needs_an_observed_pocket() {
        assert_eq!(ball_count(&GameState::default()), None);
        let state = DefaultReducer.reduce(
            &GameState::default(),
            &[EventRecord {
                frame_id: 1,
                event: GameEvent::PocketObserved {
                    pocket: Pocket::PokeBalls,
                    items: vec![
                        ("ITEM_POKE_BALL".into(), 4),
                        ("ITEM_MASTER_BALL".into(), 1),
                        ("ITEM_GREAT_BALL".into(), 2),
                    ],
                },
            }],
        );
        assert_eq!(ball_count(&state), Some(6));
        // A tracked (stale) count is still a known count.
        let state = DefaultReducer.reduce(
            &state,
            &[EventRecord {
                frame_id: 2,
                event: GameEvent::ItemsChanged {
                    pocket: Pocket::PokeBalls,
                    item: "ITEM_POKE_BALL".into(),
                    delta: -1,
                    reason: "thrown".into(),
                },
            }],
        );
        assert_eq!(ball_count(&state), Some(5));
    }

    #[test]
    fn buying_rules() {
        assert!(must_buy(Some(4)));
        assert!(!must_buy(Some(SHINY_RESERVE)));
        assert!(!must_buy(None));
        assert!(should_buy(None));
        assert!(should_buy(Some(14)));
        assert!(!should_buy(Some(15)));
    }
}
