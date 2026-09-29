//! The Poké Ball stock policy, shared by the planner and the tools: a
//! reserve of [`SHINY_RESERVE`] balls only a shiny may use, restocks toward
//! [`TARGET_STOCK`], and the price of [`POTIONS_KEPT`] Potions never spent.
//! One rule on both sides: the planner never plans a catch the tools won't
//! buy the balls for (fleet workers 2 and 5: Catch(MANKEY, 7 balls) with
//! ₽680 planned 14 cycles in a row; keeping the Potion money the mart sold
//! none, and the hunt failed each time).

use pokebot_gamedata::GameData;

/// Balls only a shiny may use.
pub const SHINY_RESERVE: u16 = 5;
/// Stock a mart visit restocks to.
pub const TARGET_STOCK: u16 = 15;
/// Potions whose price is never spent on balls.
pub const POTIONS_KEPT: u32 = 2;

fn price(data: &GameData, item: &str) -> Option<u32> {
    data.items.get(item).map(|i| i.price).filter(|p| *p > 0)
}

/// Money that may be spent: all but the price of [`POTIONS_KEPT`] Potions.
pub fn spendable(data: &GameData, money: u32) -> u32 {
    let potions = price(data, "ITEM_POTION").map_or(0, |p| POTIONS_KEPT * p);
    money.saturating_sub(potions)
}

/// How many of `item` `money` pays for while keeping the Potion money. 0
/// when either price is missing from the data.
pub fn affordable(data: &GameData, item: &str, money: u32) -> u16 {
    let (Some(_), Some(p)) = (price(data, "ITEM_POTION"), price(data, item)) else {
        return 0;
    };
    u16::try_from(spendable(data, money) / p).unwrap_or(u16::MAX)
}

/// Money a catch may spend on the balls it throws, with `held` balls in the
/// bag: the spendable money less the price of topping the held balls up to
/// the shiny reserve (only balls above it are ever thrown).
pub fn ball_budget(data: &GameData, money: u32, held: u16) -> u32 {
    let Some(p) = price(data, "ITEM_POKE_BALL") else {
        return 0;
    };
    let top_up = u32::from(SHINY_RESERVE.saturating_sub(held)) * p;
    spendable(data, money).saturating_sub(top_up)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    /// Fleet worker 2: ₽680, no balls. Two Potions (₽600) are kept, and the
    /// five-ball reserve would cost ₽1000 more: nothing is left to throw.
    #[test]
    fn a_catch_budget_keeps_potions_and_the_shiny_reserve() {
        let Some(d) = data() else { return };
        assert_eq!(spendable(&d, 680), 80);
        assert_eq!(affordable(&d, "ITEM_POKE_BALL", 680), 0);
        assert_eq!(ball_budget(&d, 680, 0), 0);
        // Five held: the reserve is full, the spendable money is the budget.
        assert_eq!(ball_budget(&d, 2000, 5), 1400);
        // Two held: three more for the reserve (₽600) first.
        assert_eq!(ball_budget(&d, 2000, 2), 800);
    }
}
