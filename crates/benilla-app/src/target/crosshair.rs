//! classiccraft (fork only): the external driver's crosshair as WoW's cursor (2026-10-03). In
//! crosshair mode the picks aim at the screen centre (`external::pick_point`); here the frame's
//! pick is published as [`external::CrosshairTarget`] (what a right-click would do), and the
//! driver's right-click ([`external::CrosshairUse`]) is latched as a press on that pick and played
//! as WoW's own right-click ([`WorldRightClick`] -> `click::act_on_right_click`): talking, quests,
//! vendors, using objects, reading - and WoW's own range refusals.

use super::*;
use crate::player::external::{self, CrosshairTarget, CrosshairUse};

#[allow(clippy::too_many_arguments)]
pub(super) fn crosshair_target_and_click(
    hovered: Res<Hovered>,
    object: Res<HoveredObject>,
    occlusion: Res<PickOcclusion>,
    cursor: Res<cursor_mode::WorldCursor>,
    attack_fork: Res<cursor_mode::AttackFork>,
    names: Res<crate::names::NameCache>,
    go_templates: Res<crate::go_templates::GameObjectTemplates>,
    stores: Query<&ObjectStore>,
    mut target: ResMut<CrosshairTarget>,
    mut use_: ResMut<CrosshairUse>,
    mut press: ResMut<PressPick>,
    mut clicks: MessageWriter<WorldRightClick>,
) {
    let on = external::crosshair();
    let next = if !on {
        CrosshairTarget::default()
    } else if go_is_nearest(&hovered, &object) {
        let guid = object.guid.unwrap_or(0);
        CrosshairTarget {
            guid,
            kind: kind_code(cursor.kind),
            name: go_templates.get(guid).map(|t| t.name.clone()).unwrap_or_default(),
            distance: object.distance,
            unable: cursor.unable,
        }
    } else if let (Some(entity), Some(guid)) = (hovered.target, hovered.guid) {
        CrosshairTarget {
            guid,
            kind: kind_code(cursor.kind),
            name: names.peek_unit(guid, stores.get(entity).ok()).unwrap_or_default().to_string(),
            distance: hovered.distance,
            unable: cursor.unable,
        }
    } else {
        CrosshairTarget::default()
    };
    if *target != next {
        *target = next;
    }
    if std::mem::take(&mut use_.pending) && on && target.kind != 0 {
        *press = PressPick {
            hovered: *hovered,
            object: *object,
            occlusion: *occlusion,
            cursor: *cursor,
            attack_fork: *attack_fork,
        };
        clicks.write(WorldRightClick);
        info!(
            "classiccraft: crosshair right-click on {:?} ({:#x}, {})",
            target.name,
            target.guid,
            external::CROSSHAIR_KINDS[target.kind as usize]
        );
    }
}

/// WoW's cursor as [`external::CROSSHAIR_KINDS`]' index; Point (nothing to do) is 0.
fn kind_code(kind: cursor_mode::CursorKind) -> u8 {
    use cursor_mode::CursorKind as K;
    match kind {
        K::Point => 0,
        K::Attack => 1,
        K::Speak => 2,
        K::Pickup | K::LootAll => 3,
        K::Interact => 4,
        K::Buy => 5,
        K::Inspect => 6,
        K::Trainer => 7,
        K::Taxi => 8,
        K::Skin => 9,
        K::Mail => 10,
        K::Mine => 11,
        K::GatherHerbs => 12,
        K::PickLock => 13,
        K::Repair => 14,
        K::Cast => 15,
    }
}
