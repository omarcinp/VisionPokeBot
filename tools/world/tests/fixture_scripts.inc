.equ LADY_READY, VAR_TEMP_2

Fixture_MapScripts::
	map_script MAP_SCRIPT_ON_TRANSITION, Fixture_OnTransition
	map_script MAP_SCRIPT_ON_FRAME_TABLE, Fixture_OnFrame
	.byte 0

Fixture_OnTransition::
	setworldmapflag FLAG_WORLD_MAP_PALLET_TOWN
	end

Fixture_OnFrame::
	map_script_2 VAR_MAP_SCENE_PALLET_TOWN_OAK, 2, Fixture_EventScript_Scene
	.2byte 0

Fixture_EventScript_Scene::
	lockall
	msgbox Fixture_Text_Hello @ trailing comment
	releaseall
	end

@ A gym leader, as in PewterCity_Gym.
Fixture_EventScript_Leader::
	famechecker FAMECHECKER_BROCK, FCPICKSTATE_COLORED, UpdatePickStateFromSpecialVar8005
	trainerbattle_single TRAINER_LEADER_BROCK, Fixture_Text_Intro, Fixture_Text_Defeat, Fixture_EventScript_Defeated, NO_MUSIC
	goto_if_unset FLAG_GOT_TM39_FROM_BROCK, Fixture_EventScript_GiveTM
	msgbox Fixture_Text_PostBattle
	release
	end

Fixture_EventScript_Defeated::
	setflag FLAG_DEFEATED_BROCK
	setflag FLAG_BADGE01_GET
	setvar VAR_MAP_SCENE_PEWTER_CITY, 1
	set_gym_trainers 1
	goto Fixture_EventScript_GiveTM
	end

Fixture_EventScript_GiveTM::
	msgbox Fixture_Text_TakeThis
	checkitemspace ITEM_TM39
	goto_if_eq VAR_RESULT, FALSE, Fixture_EventScript_NoRoom
	giveitem_msg Fixture_Text_ReceivedTM, ITEM_TM39
	setflag FLAG_GOT_TM39_FROM_BROCK
	release
	end

Fixture_EventScript_NoRoom::
	msgbox Fixture_Text_NoRoom
	release
	end

Fixture_EventScript_Lady::
	lock
	goto_if_eq LADY_READY, TRUE, Fixture_EventScript_LadyReady
	msgbox Fixture_Text_Hmm, MSGBOX_YESNO
	goto_if_eq VAR_RESULT, YES, Fixture_EventScript_LadyYes
	call Fixture_EventScript_Helper
	release
	end

Fixture_EventScript_Helper::
	setflag FLAG_TEMP_2
	return

Fixture_EventScript_LadyReady::
	msgbox Fixture_Text_Ready
	release
	end

Fixture_EventScript_LadyYes::
	msgbox Fixture_Text_Yes
	goto Fixture_EventScript_Lady
	end

.ifdef FIRERED
Fixture_EventScript_Version::
	setvar VAR_TEMP_1, SPECIES_SCYTHER
	givemon VAR_TEMP_1, 25
	end
.else
Fixture_EventScript_Version::
	setvar VAR_TEMP_1, SPECIES_PINSIR
	givemon VAR_TEMP_1, 25
	end
.endif

Fixture_EventScript_Clerk::
	pokemart Fixture_Items
	end

@ A Pokédex aide, as in Route2_EastBuilding: the dex count lands in
@ VAR_0x8005 (seen) and VAR_0x8006 (caught) and is copied around.
Fixture_EventScript_Aide::
	lock
	setvar VAR_0x8004, 0
	specialvar VAR_RESULT, GetPokedexCount
	copyvar VAR_0x8008, VAR_0x8005
	copyvar VAR_0x8009, VAR_0x8006
	buffernumberstring STR_VAR_3, VAR_0x8006
	goto_if_lt VAR_0x8009, 10, Fixture_EventScript_NotEnough
	goto_if_eq VAR_RESULT, TRUE, Fixture_EventScript_National
	compare VAR_0x8008, 20
	goto_if_ge Fixture_EventScript_SeenPlenty
	giveitem ITEM_HM05
	release
	end

Fixture_EventScript_NotEnough::
	msgbox Fixture_Text_NoRoom
	release
	end

Fixture_EventScript_National::
	setvar VAR_0x8004, 1
	specialvar VAR_RESULT, GetPokedexCount
	goto_if_lt VAR_0x8006, 60, Fixture_EventScript_NotEnough
	release
	end

Fixture_EventScript_SeenPlenty::
	copyvar VAR_0x8009, 5
	goto_if_eq VAR_0x8009, 5, Fixture_EventScript_NotEnough
	release
	end

@ Money, party size, coins in a temp var, a species in the party.
Fixture_EventScript_Vendor::
	checkmoney 500
	goto_if_eq VAR_RESULT, FALSE, Fixture_EventScript_NotEnough
	getpartysize
	goto_if_eq VAR_RESULT, PARTY_SIZE, Fixture_EventScript_NotEnough
	checkcoins VAR_TEMP_1
	goto_if_ge VAR_TEMP_1, 9990, Fixture_EventScript_NotEnough
	setvar VAR_0x8004, SPECIES_MAGIKARP
	specialvar VAR_RESULT, DoesPlayerPartyContainSpecies
	goto_if_eq VAR_RESULT, FALSE, Fixture_EventScript_NotEnough
	specialvar VAR_RESULT, CountPartyNonEggMons
	goto_if_eq VAR_RESULT, 1, Fixture_EventScript_NotEnough
	specialvar VAR_RESULT, HasAllKantoMons
	goto_if_eq VAR_RESULT, TRUE, Fixture_EventScript_NotEnough
	release
	end

	.align 2
Fixture_Items::
	.2byte ITEM_POKE_BALL
	.2byte ITEM_POTION
	.2byte ITEM_NONE
	release
	end

Fixture_Text_Hello::
	.string "Hello, {PLAYER}!\n"
	.string "Wild POKéMON live in tall grass!\p"
	.string "{STR_VAR_1} used {STR_VAR_2}!$"

Fixture_Text_Intro::
	.string "So, you're here.\l"
	.string "I'm BROCK!$"

Fixture_Text_Defeat::
	.string "I took you for granted.$"

Fixture_Text_PostBattle::
	.string "Later.$"

Fixture_Text_TakeThis::
	.string "Take this.$"

Fixture_Text_ReceivedTM::
	.string "Received TM39.$"

Fixture_Text_NoRoom::
	.string "No room.$"

Fixture_Text_Hmm::
	.string "Hmm?$"

Fixture_Text_Ready::
	.string "Ready.$"

Fixture_Text_Yes::
	.string "Yes.$"
