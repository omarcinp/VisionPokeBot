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
    /// The evolution scene ("What? BULBASAUR is evolving!" in the battle
    /// text box over the evolution animation). B here cancels the
    /// evolution; its text is read as dialogue.
    Evolution,
    LearnMove,
    EggHatching,
    Naming,
    SaveMenu,
    /// Fades, battle wipes, cut-ins, the dungeon map preview: nothing on
    /// screen is state, wait for it to end.
    Transition,
    /// Boot and new-game intro scenes without a text box: the copyright
    /// notice, the Game Freak shooting star, Professor Oak's stage between
    /// his speech pages. Start (boot) or A (Oak) moves them on.
    Intro,
    /// The Quest Log recap FireRed plays after CONTINUE ("Previously on your
    /// quest…" over a grey replay of the last actions). B skips it.
    QuestLog,
    /// The white-out screens after the last Pokémon fainted ("RED scurried
    /// back home, protecting the exhausted and fainted POKéMON…": white
    /// text on black, no box).
    Whiteout,
    #[default]
    Unknown,
}
