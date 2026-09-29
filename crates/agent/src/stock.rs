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

// The policy's numbers and budget are the planner's too (one rule on both
// sides): see [`pokebot_planner::stock`].
pub use pokebot_planner::stock::{
    affordable, ball_budget, spendable, POTIONS_KEPT, SHINY_RESERVE, TARGET_STOCK,
};

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
    let need = TARGET_STOCK.saturating_sub(stock);
    need.min(affordable(data, "ITEM_POKE_BALL", money))
}

/// The stock is known to hold no ball above the shiny reserve, so the catch
/// policy can't throw one: buy before going on. At exactly the reserve
/// nothing is ever thrown, so the count never drops below it (Switch, 5
/// balls: every catch declined, never restocked).
pub fn must_buy(stock: Option<u16>) -> bool {
    stock.is_some_and(|n| n <= SHINY_RESERVE)
}

/// Potions held (the Items pocket); `None` when the pocket is unknown.
pub fn potion_count(state: &GameState) -> Option<u16> {
    let items = state
        .bag
        .pockets
        .get(&pokebot_state::Pocket::Items)?
        .value
        .as_ref()?;
    Some(
        items
            .iter()
            .filter(|(item, _)| item == "ITEM_POTION")
            .fold(0u16, |sum, (_, n)| sum.saturating_add(*n)),
    )
}

/// Potions to buy on a mart visit: up to [`POTIONS_KEPT`], from the money
/// kept for them (fleet worker 2 kept that money, bought none, and met Mt.
/// Moon's trainers with nothing to heal with: whited out on the way back
/// to the Center). Nothing when the pocket is unknown.
pub fn potions_to_buy(data: &GameData, held: Option<u16>, money: u32) -> u16 {
    let Some(held) = held else { return 0 };
    let Some(price) = data
        .items
        .get("ITEM_POTION")
        .map(|i| i.price)
        .filter(|p| *p > 0)
    else {
        return 0;
    };
    let need = u16::try_from(POTIONS_KEPT)
        .unwrap_or(0)
        .saturating_sub(held);
    need.min(u16::try_from(money / price).unwrap_or(u16::MAX))
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
    fn potions_are_topped_up_to_the_kept_two_from_the_money_kept() {
        let Some(data) = data() else { return };
        // ¥600 is the kept money itself: it buys the two.
        assert_eq!(potions_to_buy(&data, Some(0), 600), 2);
        assert_eq!(potions_to_buy(&data, Some(1), 5000), 1);
        assert_eq!(potions_to_buy(&data, Some(2), 5000), 0);
        assert_eq!(potions_to_buy(&data, Some(0), 299), 0);
        assert_eq!(potions_to_buy(&data, None, 5000), 0);
    }

    #[test]
    fn a_stock_at_the_reserve_must_be_restocked() {
        assert!(must_buy(Some(SHINY_RESERVE)));
        assert!(must_buy(Some(0)));
        assert!(!must_buy(Some(SHINY_RESERVE + 1)));
        assert!(!must_buy(None));
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
        assert!(should_buy(None));
        assert!(should_buy(Some(14)));
        assert!(!should_buy(Some(15)));
    }
}
