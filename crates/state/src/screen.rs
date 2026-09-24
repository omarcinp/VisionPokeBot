use serde::{Deserialize, Serialize};

/// Which kind of screen is showing. `Unknown` is a legitimate answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ScreenState {
    TitleScreen,
    MainMenu,
    /// Full-screen information page (new-game tutorial).
    InfoPage,
    /// A list menu without a message box (e.g. the Start menu).
    Menu,
    Overworld,
    Dialogue,
    StartMenu,
    Bag,
    PartyMenu,
    PokemonSummary,
    PokemonMoves,
    PcStorage,
    Shop,
    PokemonCenter,
    DaycareDialogue,
    BattleIntro,
    BattleText,
    BattleCommand,
    BattleMoveSelection,
    BattlePokemonSelection,
    BattleBag,
    Evolution,
    LearnMove,
    EggHatching,
    Naming,
    SaveMenu,
    Transition,
    #[default]
    Unknown,
}
