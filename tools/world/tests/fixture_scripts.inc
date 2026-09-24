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
