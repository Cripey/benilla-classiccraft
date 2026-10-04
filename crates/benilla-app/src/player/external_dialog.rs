//! classiccraft: WoW's NPC windows as the external driver's own screens (2026-10-03).
//!
//! The foundation for quests, gossip and vendors from Minecraft mode: while the driver has the
//! crosshair (`external::crosshair`), the open gossip menu, questgiver panel or vendor goes out as
//! a [`DialogOut`] whenever it changes (the same snapshots WoW's Lua frames read; the WoW frames
//! themselves are hidden by the crate), and the driver's choices come back as [`DialogIn`], queued
//! as the very intents those Lua frames would queue - so WoW's own logic (sessions, refusals,
//! re-opens) runs unchanged. A turned-in quest's rewards go out as [`DialogOut`] kind
//! [`DIALOG_QUEST_DONE`] for the driver's own rewards.

use benilla_protocol::SessionEvent;
use benilla_ui::script::{QuestAction, QuestItemView, QuestPanel, QuestSelect, UiScript};
use bevy::prelude::*;

use crate::items::Items;
use crate::net::NetCommands;

/// [`DialogOut::kind`]: nothing open any more.
pub const DIALOG_CLOSE: u8 = 0;
/// A gossip menu: `options` = its quest rows, then its gossip options.
pub const DIALOG_GOSSIP: u8 = 1;
/// A questgiver greeting: `options` = active quests, then available ones.
pub const DIALOG_QUEST_GREETING: u8 = 2;
/// A quest's accept panel.
pub const DIALOG_QUEST_DETAIL: u8 = 3;
/// A quest's turn-in progress panel (`completable` = Continue allowed).
pub const DIALOG_QUEST_PROGRESS: u8 = 4;
/// A quest's reward panel.
pub const DIALOG_QUEST_REWARD: u8 = 5;
/// A vendor's goods (the driver sells its own).
pub const DIALOG_VENDOR: u8 = 6;
/// A quest turned in: `quest_id`, `money`, `rewards` (what the server granted), `xp` in `count`.
pub const DIALOG_QUEST_DONE: u8 = 7;

/// A WoW confirmation popup (`StaticPopup1..4`: "Make this inn your home?"...), forwarded and
/// answered by the classiccraft crate itself: `npc` = the popup's number, `options` = its buttons.
pub const DIALOG_CONFIRM: u8 = 8;

/// The quest log (`QuestLogOut`'s choices come back as [`DialogIn`] of this kind): `ACT_SELECT`
/// uses the bag item `arg` = bag << 8 | slot, `ACT_DECLINE` abandons quest `arg`.
pub const DIALOG_QUEST_LOG: u8 = 9;

/// [`DialogIn::action`]s.
pub const ACT_SELECT: u8 = 1;
pub const ACT_ACCEPT: u8 = 2;
pub const ACT_DECLINE: u8 = 3;
pub const ACT_CONTINUE: u8 = 4;
pub const ACT_COMPLETE: u8 = 5;
pub const ACT_CLOSE: u8 = 6;

/// An item row (reward, choice, requirement), with what the driver maps it by.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DialogItem {
    pub item_id: u32,
    pub count: u32,
    pub quality: u32,
    pub class: u32,
    pub subclass: u32,
    pub inventory_type: u32,
    pub item_level: u32,
    pub required_level: u32,
    pub name: String,
    /// classiccraft (2026-10-04): the item's stats `(ItemModType, value)`, resistances (holy, fire,
    /// nature, frost, shadow, arcane) and damage school - the driver sets enchantments from them.
    pub stats: Vec<(u32, i32)>,
    pub resistances: [i32; 6],
    pub dmg_type: u32,
}

/// An NPC window for the driver to show.
#[derive(bevy::ecs::message::Message, Clone, Debug, Default, PartialEq)]
pub struct DialogOut {
    pub npc: u64,
    pub kind: u8,
    pub npc_name: String,
    pub quest_id: u32,
    pub title: String,
    pub text: String,
    pub objectives: String,
    /// `(icon, label)`: icon 0 gossip, 1 available quest, 2 active quest, 3 vendor, 4 other.
    pub options: Vec<(u32, String)>,
    pub choices: Vec<DialogItem>,
    pub rewards: Vec<DialogItem>,
    pub required: Vec<DialogItem>,
    pub money: u32,
    pub completable: bool,
    /// [`DIALOG_QUEST_DONE`]: the XP granted.
    pub xp: u32,
}

/// One quest in [`QuestLogOut`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuestLogQuest {
    pub quest_id: u32,
    pub level: u32,
    /// 1 complete, -1 failed, 0 in progress.
    pub complete: i32,
    pub title: String,
    /// `(finished, "Kobold Vermin slain: 3/10")`.
    pub objectives: Vec<(bool, String)>,
    pub description: String,
    pub objectives_text: String,
    pub reward_money: u32,
}

/// A quest item in the WoW bags (quest class, a quest starter, or readable).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuestLogItem {
    pub bag: u32,
    pub slot: u32,
    pub item_id: u32,
    pub count: u32,
    /// It has an on-use effect (or pages to read): the log offers "Use".
    pub usable: bool,
    pub name: String,
}

/// WoW's quest log and the quest items in the WoW bags, for the driver's own quest log (2026-10-03,
/// user: quest items live only in that log, not as Minecraft items). Sent whenever it changes.
#[derive(bevy::ecs::message::Message, Clone, Debug, Default, PartialEq)]
pub struct QuestLogOut {
    pub quests: Vec<QuestLogQuest>,
    pub items: Vec<QuestLogItem>,
}

/// The driver's choice in a [`DialogOut`] window: `action` an `ACT_*`, `arg` the 0-based option
/// (`ACT_SELECT`) or reward choice (`ACT_COMPLETE`).
#[derive(bevy::ecs::message::Message, Clone, Copy, Debug)]
pub struct DialogIn {
    pub npc: u64,
    pub kind: u8,
    pub action: u8,
    pub arg: u32,
}

/// Gossip icons the driver tells apart (`GossipOptionView::icon_type`).
fn gossip_icon(kind: &str) -> u32 {
    match kind {
        "vendor" => 3,
        "gossip" => 0,
        _ => 4,
    }
}

fn item_row(v: &QuestItemView, items: &Items, commands: &NetCommands) -> Option<DialogItem> {
    let t = items.template(v.item_id, 0, commands)?;
    Some(DialogItem {
        item_id: v.item_id,
        count: v.count,
        quality: t.quality,
        class: t.class,
        subclass: t.subclass,
        inventory_type: t.inventory_type,
        item_level: t.item_level,
        required_level: t.required_level,
        name: t.name.clone(),
        stats: t.stats.clone(),
        resistances: t.resistances,
        dmg_type: t.dmg_type,
    })
}

/// Every row resolved, or `None` while a template is in flight (sent the next frame).
fn item_rows(views: &[QuestItemView], items: &Items, commands: &NetCommands) -> Option<Vec<DialogItem>> {
    views.iter().filter(|v| v.item_id != 0).map(|v| item_row(v, items, commands)).collect()
}

#[allow(clippy::too_many_arguments)]
fn forward_dialogs(
    script: Option<NonSend<UiScript>>,
    gossip: Res<crate::ui_gossip::GossipState>,
    quest: Res<crate::ui_quest::QuestGiver>,
    merchant: Res<crate::ui_merchant::MerchantOpen>,
    target: Res<super::external::CrosshairTarget>,
    items: Res<Items>,
    commands: Option<Res<NetCommands>>,
    mut last: Local<Option<DialogOut>>,
    mut name_of: Local<(u64, String)>,
    mut out: MessageWriter<DialogOut>,
) {
    if !super::external::crosshair() {
        *last = None;
        return;
    }
    let (Some(script), Some(commands)) = (script, commands) else {
        return;
    };
    // The NPC's name, from the crosshair that opened it.
    if target.guid != 0 && target.guid != name_of.0 {
        *name_of = (target.guid, target.name.clone());
    }
    let mut d = DialogOut::default();
    if let (Some(npc), Some(q)) = (quest.npc, script.quest()) {
        d.npc = npc;
        d.title = q.title.clone();
        d.text = q.body.clone();
        d.objectives = q.objectives.clone();
        d.money = q.reward_money;
        d.completable = q.completable;
        d.quest_id = match quest.view.as_ref() {
            Some(crate::ui_quest::QuestView::Detail(v)) => v.quest_id,
            Some(crate::ui_quest::QuestView::Progress(v)) => v.quest_id,
            Some(crate::ui_quest::QuestView::Reward(v)) => v.quest_id,
            _ => 0,
        };
        match q.panel {
            QuestPanel::Greeting => {
                d.kind = DIALOG_QUEST_GREETING;
                d.text = q.greeting.clone();
                d.options.extend(q.active_titles.iter().map(|t| (2, t.clone())));
                d.options.extend(q.available_titles.iter().map(|t| (1, t.clone())));
            }
            QuestPanel::Detail => d.kind = DIALOG_QUEST_DETAIL,
            QuestPanel::Progress => d.kind = DIALOG_QUEST_PROGRESS,
            QuestPanel::Reward => d.kind = DIALOG_QUEST_REWARD,
        }
        let (Some(choices), Some(rewards), Some(required)) = (
            item_rows(&q.choices, &items, &commands),
            item_rows(&q.rewards, &items, &commands),
            item_rows(&q.required, &items, &commands),
        ) else {
            return; // a template in flight
        };
        d.choices = choices;
        d.rewards = rewards;
        d.required = required;
    } else if let (Some(npc), Some(menu)) = (gossip.npc, script.gossip()) {
        d.npc = npc;
        d.kind = DIALOG_GOSSIP;
        d.text = menu.greeting.clone();
        d.options.extend(menu.quests.iter().map(|r| (if r.active { 2 } else { 1 }, r.title.clone())));
        d.options.extend(menu.options.iter().map(|o| (gossip_icon(&o.icon_type), o.label.clone())));
    } else if let Some(npc) = merchant.vendor {
        d.npc = npc;
        d.kind = DIALOG_VENDOR;
    } else if last.as_ref().is_some_and(|l| l.kind != DIALOG_CLOSE) {
        d.npc = last.as_ref().map_or(0, |l| l.npc);
        d.kind = DIALOG_CLOSE;
    } else {
        return;
    }
    if d.npc == name_of.0 {
        d.npc_name = name_of.1.clone();
    }
    if last.as_ref() != Some(&d) {
        info!("classiccraft: dialog kind {} for {:#x} ({:?}) to the driver", d.kind, d.npc, d.title);
        out.write(d.clone());
        *last = Some(d);
    }
}

/// A turned-in quest, for the driver's own rewards.
fn forward_quest_done(
    In(ev): In<SessionEvent>,
    items: Res<Items>,
    commands: Option<Res<NetCommands>>,
    mut out: MessageWriter<DialogOut>,
) {
    let SessionEvent::QuestComplete(c) = ev else {
        return;
    };
    if !super::external::crosshair() {
        return;
    }
    let rewards = c
        .items
        .iter()
        .map(|&(item_id, count)| {
            let t = commands.as_ref().and_then(|cmd| items.template(item_id, 0, cmd));
            DialogItem {
                item_id,
                count,
                quality: t.map_or(1, |t| t.quality),
                class: t.map_or(0, |t| t.class),
                subclass: t.map_or(0, |t| t.subclass),
                inventory_type: t.map_or(0, |t| t.inventory_type),
                item_level: t.map_or(0, |t| t.item_level),
                required_level: t.map_or(0, |t| t.required_level),
                name: t.map_or_else(String::new, |t| t.name.clone()),
                stats: t.map_or_else(Vec::new, |t| t.stats.clone()),
                resistances: t.map_or([0; 6], |t| t.resistances),
                dmg_type: t.map_or(0, |t| t.dmg_type),
            }
        })
        .collect();
    info!("classiccraft: quest {} turned in ({} XP, {} copper) to the driver", c.quest_id, c.xp, c.money);
    out.write(DialogOut {
        kind: DIALOG_QUEST_DONE,
        quest_id: c.quest_id,
        money: c.money,
        xp: c.xp,
        rewards,
        ..Default::default()
    });
}

/// The quest log and the bags' quest items, whenever either changes (the bags are polled twice a
/// second through the stock container verbs).
fn forward_quest_log(
    script: Option<NonSend<UiScript>>,
    items: Res<Items>,
    commands: Option<Res<NetCommands>>,
    mut bag_items: Local<Vec<QuestLogItem>>,
    mut frames: Local<u32>,
    mut last: Local<Option<QuestLogOut>>,
    mut out: MessageWriter<QuestLogOut>,
) {
    let (Some(script), Some(commands)) = (script, commands) else {
        return;
    };
    *frames = frames.wrapping_add(1);
    if *frames % 30 == 1 {
        // "bag,slot,id,count;..." of every bag item.
        let listing = script
            .eval::<String>(
                r#"local out = {}
                for bag = 0, 4 do
                    for slot = 1, GetContainerNumSlots(bag) or 0 do
                        local link = GetContainerItemLink(bag, slot)
                        if link then
                            local _, _, id = string.find(link, "item:(%d+)")
                            local _, count = GetContainerItemInfo(bag, slot)
                            table.insert(out, bag .. "," .. slot .. "," .. (id or 0) .. "," .. (count or 1))
                        end
                    end
                end
                return table.concat(out, ";")"#,
            )
            .unwrap_or_default();
        let mut found = Vec::new();
        for row in listing.split(';').filter(|r| !r.is_empty()) {
            let v: Vec<u32> = row.split(',').filter_map(|x| x.parse().ok()).collect();
            let [bag, slot, item_id, count] = v[..] else {
                continue;
            };
            let Some(t) = items.template(item_id, 0, &commands) else {
                continue; // template in flight: next poll
            };
            let usable = t.use_spell.is_some() || t.spells.iter().any(|sp| sp.trigger == 0 && sp.spell_id != 0)
                || t.page_text != 0;
            if t.class == 12 || t.start_quest != 0 {
                found.push(QuestLogItem { bag, slot, item_id, count, usable, name: t.name.clone() });
            }
        }
        *bag_items = found;
    }
    let log = script.quest_log();
    let quests = log
        .entries
        .iter()
        .filter(|e| !e.is_header)
        .map(|e| QuestLogQuest {
            quest_id: e.quest_id,
            level: e.level,
            complete: e.complete,
            title: e.title.clone(),
            objectives: e.objectives.iter().map(|o| (o.finished, o.text.clone())).collect(),
            description: e.detail.as_ref().map_or_else(String::new, |d| d.description.clone()),
            objectives_text: e.detail.as_ref().map_or_else(String::new, |d| d.objectives_text.clone()),
            reward_money: e.detail.as_ref().map_or(0, |d| d.reward_money),
        })
        .collect();
    let now = QuestLogOut { quests, items: bag_items.clone() };
    // Also every 5 s unchanged: a Minecraft started (or reconnected) later gets it too.
    let changed = last.as_ref() != Some(&now);
    if changed || *frames % 300 == 0 {
        if changed {
            info!("classiccraft: quest log ({} quests, {} quest items) to the driver", now.quests.len(), now.items.len());
        }
        out.write(now.clone());
        *last = Some(now);
    }
}

/// The driver's choices, queued as WoW's Lua frames would queue them.
fn apply_dialogs(
    mut input: MessageReader<DialogIn>,
    script: Option<NonSendMut<UiScript>>,
    gossip: Res<crate::ui_gossip::GossipState>,
    quest: Res<crate::ui_quest::QuestGiver>,
    merchant: Res<crate::ui_merchant::MerchantOpen>,
) {
    let Some(mut script) = script else {
        input.clear();
        return;
    };
    for d in input.read() {
        if d.kind == DIALOG_CONFIRM {
            continue; // the crate answers its popups
        }
        if d.kind == DIALOG_QUEST_LOG {
            match d.action {
                ACT_SELECT => {
                    let (bag, slot) = (d.arg >> 8, d.arg & 0xFF);
                    info!("classiccraft: quest item in bag {bag} slot {slot} used from Minecraft");
                    let _ = script.eval::<()>(&format!("UseContainerItem({bag}, {slot})"));
                }
                ACT_DECLINE => {
                    info!("classiccraft: quest {} abandoned from Minecraft", d.arg);
                    script.push_quest_log_abandon(d.arg);
                }
                _ => {}
            }
            continue;
        }
        // A screen left over from a window that has gone (or another NPC's): nothing to act on.
        if ![gossip.npc, quest.npc, merchant.vendor].contains(&Some(d.npc)) {
            info!("classiccraft: dialog choice {:?} for a closed window - ignored", d);
            continue;
        }
        info!("classiccraft: dialog choice {:?}", d);
        match (d.kind, d.action) {
            (DIALOG_GOSSIP, ACT_SELECT) => {
                // Quest rows first, then the options (DialogOut's order).
                let quests = gossip.quests.len() as u32;
                if d.arg < quests {
                    script.push_gossip_quest_select(d.arg + 1);
                } else {
                    script.push_gossip_select(d.arg - quests + 1);
                }
            }
            (DIALOG_GOSSIP, ACT_CLOSE) => script.push_gossip_close(),
            (DIALOG_QUEST_GREETING, ACT_SELECT) => {
                let active = script.quest().map_or(0, |q| q.active_titles.len()) as u32;
                let select = if d.arg < active {
                    QuestSelect { active: true, index: d.arg + 1 }
                } else {
                    QuestSelect { active: false, index: d.arg - active + 1 }
                };
                script.push_quest_select(select);
            }
            (_, ACT_ACCEPT) => script.push_quest_action(QuestAction::Accept),
            (_, ACT_DECLINE) => script.push_quest_action(QuestAction::Decline),
            (_, ACT_CONTINUE) => script.push_quest_action(QuestAction::Continue),
            (_, ACT_COMPLETE) => script.push_quest_action(QuestAction::Reward(d.arg)),
            (DIALOG_VENDOR, ACT_CLOSE) => script.push_merchant_close(),
            (_, ACT_CLOSE) => script.push_quest_action(QuestAction::Close),
            _ => {}
        }
    }
}

pub(super) fn plugin(app: &mut App) {
    use crate::net::handlers::NetHandlerApp;
    app.add_message::<DialogOut>()
        .add_message::<DialogIn>()
        .add_message::<QuestLogOut>()
        .add_systems(Update, forward_quest_log.after(crate::ui_script::UiFeed))
        .add_systems(
            Update,
            (
                forward_dialogs.after(crate::ui_script::UiFeed),
                apply_dialogs.before(crate::ui_script::UiInput),
            ),
        )
        .net_handler(benilla_protocol::SessionEventKind::QuestComplete, forward_quest_done);
}
