//! The input bridge: while Minecraft drives, benilla's window keeps focus and every key, button,
//! wheel notch and raw mouse motion goes into Minecraft's input ring
//! (`protocol/mcwow_overlay_protocol.h`, the overlay file Minecraft creates), and benilla's own
//! input is cleared before any of its systems read it. **`** (grave) switches to WoW UI mode, where
//! benilla gets its input back and the cursor is free; **Numpad+** turns the whole bridge on or off.

use crate::bridge::Bridge;
use crate::overlay::OverlayFile;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::mouse::{MouseButtonInput, MouseMotion, MouseScrollUnit, MouseWheel};
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

const OFF_READER_HEARTBEAT: usize = 40;
const OFF_BACKBUFFER_W: usize = 44;
const OFF_BACKBUFFER_H: usize = 48;
const OFF_INPUT_ACTIVE: usize = 52;

const IN_KEY: u16 = 1;
const IN_MOUSE_BUTTON: u16 = 2;
const IN_SCROLL: u16 = 3;
const IN_CURSOR: u16 = 4;
const IN_TEXT: u16 = 5;
const IN_RELEASE_ALL: u16 = 6;
const IN_LOOK: u16 = 9;

pub struct InputBridgePlugin;

impl Plugin for InputBridgePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputBridge>()
            .add_systems(
                PreUpdate,
                forward_input
                    .after(bevy::input::InputSystems)
                    .before(bevy::picking::PickingSystems::Input),
            )
            .add_systems(Update, (hide_wow_frames, forward_popups));
    }
}

/// WoW's frames Minecraft mode hides (user, 2026-10-02): the action bars (`MainMenuBar` carries the
/// bags, micro menu and XP bar too), the player and target unit frames. Minecraft's HUD stands in.
/// The chat panel too (user, 2026-10-03: WoW's chat shows in Minecraft's own now, `McwowGmChat`):
/// the seven chat windows (their scroll buttons are children), their tabs, the menu button and the
/// edit box; 1.12 parents the tabs, button and box to `UIParent`.
const HIDDEN_FRAMES: &str = r#"{"MainMenuBar", "MultiBarBottomLeft", "MultiBarBottomRight",
    "MultiBarLeft", "MultiBarRight", "PetActionBarFrame", "ShapeshiftBarFrame", "PlayerFrame",
    "TargetFrame", "ChatFrame1", "ChatFrame2", "ChatFrame3", "ChatFrame4", "ChatFrame5",
    "ChatFrame6", "ChatFrame7", "ChatFrame1Tab", "ChatFrame2Tab", "ChatFrame3Tab", "ChatFrame4Tab",
    "ChatFrame5Tab", "ChatFrame6Tab", "ChatFrame7Tab", "ChatFrameMenuButton", "ChatFrameEditBox"}"#;

/// WoW's NPC windows, shown as Minecraft screens in Minecraft mode (`external_dialog`): see-through
/// there (alpha 0), not hidden - their event handlers close the NPC's session when they can't
/// become visible (`GossipFrame.lua:13`, `QuestFrame.lua:28`, `MerchantFrame.lua:24`).
const FADED_FRAMES: &str = r#"{"GossipFrame", "QuestFrame", "MerchantFrame", "StaticPopup1", "StaticPopup2",
    "StaticPopup3", "StaticPopup4"}"#;

/// In Minecraft mode (driving, not WoW UI mode) [`HIDDEN_FRAMES`] go under a hidden frame - a
/// child of a hidden parent stays hidden through its own `Show()` (a new target re-shows
/// `TargetFrame`) - and back under their own parents in WoW UI mode or with the bridge off. Retried
/// each frame until the stock UI is up.
fn hide_wow_frames(
    bridge: Res<Bridge>,
    ib: Res<InputBridge>,
    script: Option<NonSend<benilla_ui::script::UiScript>>,
    mut applied: Local<Option<bool>>,
    mut frames: Local<u32>,
) {
    let want = bridge.driving() && !ib.wow_ui;
    *frames = frames.wrapping_add(1);
    // Hidden, re-applied about once a second: a fresh UI VM (world entry, a reload) rebuilds the
    // frames under their own parents. The chunk is idempotent.
    if *applied == Some(want) && !(want && *frames % 60 == 0) {
        return;
    }
    let first = *applied != Some(want);
    let Some(script) = script else {
        return;
    };
    let chunk = if want {
        format!(
            r#"if not (UIParent and MainMenuBar and PlayerFrame and TargetFrame) then return false end
            if not CC_Hider then
                CC_Hider = CreateFrame("Frame", "CC_Hider", UIParent)
                CC_Hider:Hide()
                CC_Parents = {{}}
            end
            for _, n in ipairs({HIDDEN_FRAMES}) do
                local f = getglobal(n)
                if f and f:GetParent() ~= CC_Hider then
                    CC_Parents[n] = f:GetParent()
                    f:SetParent(CC_Hider)
                end
            end
            for _, n in ipairs({FADED_FRAMES}) do
                local f = getglobal(n)
                if f then f:SetAlpha(0) end
            end
            return true"#
        )
    } else {
        format!(
            r#"if not CC_Hider then return true end
            for _, n in ipairs({HIDDEN_FRAMES}) do
                local f = getglobal(n)
                if f and CC_Parents[n] then
                    f:SetParent(CC_Parents[n])
                    CC_Parents[n] = nil
                end
            end
            for _, n in ipairs({FADED_FRAMES}) do
                local f = getglobal(n)
                if f then f:SetAlpha(1) end
            end
            return true"#
        )
    };
    match script.eval::<bool>(&chunk) {
        Ok(true) => {
            *applied = Some(want);
            if first {
                info!(
                    "classiccraft: WoW action bars and unit frames {}",
                    if want { "hidden" } else { "shown" }
                );
            }
        }
        Ok(false) => {} // the UI is not up yet
        Err(e) => {
            warn!("classiccraft: hiding WoW frames failed: {e}");
            *applied = Some(want); // don't retry a broken chunk every frame
        }
    }
}

#[derive(Resource, Default)]
struct InputBridge {
    /// WoW UI mode (grave): benilla keeps its input while Minecraft still drives.
    wow_ui: bool,
    /// Forwarding last frame, to send one release-all on the way out.
    forwarding: bool,
    heartbeat: u32,
    /// Sub-pixel mouse motion carried to the next event (the ring takes whole counts).
    look_rem: Vec2,
    /// The cursor is locked and hidden for look motion (no Minecraft screen open).
    locked: bool,
}

#[allow(clippy::too_many_arguments)]
fn forward_input(
    time: Res<Time>,
    mut bridge: ResMut<Bridge>,
    mut ib: ResMut<InputBridge>,
    mut file: ResMut<OverlayFile>,
    window: Single<(&Window, &mut CursorOptions), With<PrimaryWindow>>,
    mut keys: ResMut<ButtonInput<KeyCode>>,
    mut key_events: ResMut<Messages<KeyboardInput>>,
    mut buttons: ResMut<ButtonInput<MouseButton>>,
    mut button_events: ResMut<Messages<MouseButtonInput>>,
    mut motion: ResMut<Messages<MouseMotion>>,
    mut cursor_moves: ResMut<Messages<bevy::window::CursorMoved>>,
    mut wheel: ResMut<Messages<MouseWheel>>,
    mut accumulated: (
        ResMut<bevy::input::mouse::AccumulatedMouseMotion>,
        ResMut<bevy::input::mouse::AccumulatedMouseScroll>,
    ),
) {
    let ib = &mut *ib;
    let file = &mut *file;
    let (window, mut cursor) = window.into_inner();
    file.open(time.elapsed_secs());

    // The two toggles are ours in every mode and never reach Minecraft or benilla.
    if keys.just_pressed(KeyCode::NumpadAdd) {
        bridge.toggle();
    }
    let toggles = |k: KeyCode| matches!(k, KeyCode::NumpadAdd | KeyCode::Backquote);
    if keys.just_pressed(KeyCode::Backquote) && bridge.driving() {
        ib.wow_ui = !ib.wow_ui;
        info!(
            "classiccraft: {}",
            if ib.wow_ui {
                "WoW UI mode"
            } else {
                "Minecraft controls"
            }
        );
    }

    if !file.is_open() {
        return;
    }
    ib.heartbeat = ib.heartbeat.wrapping_add(1);
    let hb = ib.heartbeat;
    file.put_u32(OFF_READER_HEARTBEAT, hb);
    file.put_u32(OFF_BACKBUFFER_W, window.physical_width());
    file.put_u32(OFF_BACKBUFFER_H, window.physical_height());

    // WoW's world pick follows Minecraft's crosshair while Minecraft drives and the WoW UI is not
    // in use (talking to NPCs, using objects from Minecraft mode).
    // During a cinematic (the race intro) WoW keeps the input, as in WoW UI mode: its frame's ESC
    // is the only skip, and Minecraft has nothing to drive while the camera flies (2026-10-04).
    let cinematic = benilla_app::external::cinematic();
    benilla_app::external::set_crosshair(bridge.driving() && !ib.wow_ui && !cinematic);
    let forwarding = bridge.driving() && !ib.wow_ui && !cinematic && window.focused;
    file.put_u32(OFF_INPUT_ACTIVE, forwarding as u32);
    // Look motion wants the cursor locked; a Minecraft screen wants it free and pointed.
    let screen = file.mc_screen_open();
    let lock = forwarding && !screen;
    if lock != ib.locked {
        ib.locked = lock;
        cursor.grab_mode = if lock {
            CursorGrabMode::Locked
        } else {
            CursorGrabMode::None
        };
        cursor.visible = !lock;
    }
    if !forwarding {
        if ib.forwarding {
            file.push(IN_RELEASE_ALL, 0, 0, 0);
        }
        ib.forwarding = false;
        return;
    }
    ib.forwarding = true;

    // The cursor in Minecraft's framebuffer pixels, as the mod's screens expect it.
    let to_mc = |p: Vec2| -> (i32, i32) {
        let size = file.frame_size().unwrap_or(UVec2::new(
            window.physical_width(),
            window.physical_height(),
        ));
        let phys = p * window.scale_factor();
        let (ww, wh) = (
            window.physical_width().max(1) as f32,
            window.physical_height().max(1) as f32,
        );
        (
            (phys.x * size.x as f32 / ww) as i32,
            (phys.y * size.y as f32 / wh) as i32,
        )
    };
    let mut cursor_at = None;
    for ev in cursor_moves.drain() {
        cursor_at = Some(ev.position);
    }
    if screen {
        if let Some(p) = cursor_at.or_else(|| window.cursor_position()) {
            let (x, y) = to_mc(p);
            file.push(IN_CURSOR, 0, x, y);
        }
    }

    for ev in key_events.drain() {
        // OS auto-repeat: Minecraft tracks held keys itself.
        if toggles(ev.key_code) || ev.repeat {
            continue;
        }
        if let Some(sc) = sdl_scancode(ev.key_code) {
            file.push(IN_KEY, sc, (ev.state == ButtonState::Pressed) as i32, 0);
        }
        // The text the key typed (layout, shift and Space included), for Minecraft's text fields.
        if ev.state == ButtonState::Pressed {
            let typed = ev.text.as_deref().or(match &ev.logical_key {
                Key::Character(c) => Some(c.as_str()),
                Key::Space => Some(" "),
                _ => None,
            });
            for ch in typed
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_control())
            {
                file.push(IN_TEXT, 0, ch as i32, 0);
            }
        }
    }
    for ev in button_events.drain() {
        let code = match ev.button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            MouseButton::Back => 4,
            MouseButton::Forward => 5,
            MouseButton::Other(_) => continue,
        };
        file.push(
            IN_MOUSE_BUTTON,
            code,
            (ev.state == ButtonState::Pressed) as i32,
            0,
        );
    }
    let mut look = ib.look_rem;
    for ev in motion.drain() {
        if !screen {
            look += ev.delta;
        }
    }
    let whole = look.trunc();
    ib.look_rem = look - whole;
    if whole != Vec2::ZERO {
        file.push(IN_LOOK, 0, whole.x as i32, whole.y as i32);
    }
    for ev in wheel.drain() {
        let notches = match ev.unit {
            MouseScrollUnit::Line => ev.y,
            MouseScrollUnit::Pixel => ev.y / 40.0,
        };
        let a = (notches * 120.0).round() as i32;
        if a != 0 {
            file.push(IN_SCROLL, 0, a, 0);
        }
    }
    // benilla reads none of it.
    keys.reset_all();
    buttons.reset_all();
    *accumulated.0 = default();
    *accumulated.1 = default();
}

/// Bevy's physical key to its SDL scancode (the USB HID usage id), which the mod's input bridge
/// replays into Minecraft.
fn sdl_scancode(k: KeyCode) -> Option<u16> {
    use KeyCode::*;
    Some(match k {
        KeyA => 4,
        KeyB => 5,
        KeyC => 6,
        KeyD => 7,
        KeyE => 8,
        KeyF => 9,
        KeyG => 10,
        KeyH => 11,
        KeyI => 12,
        KeyJ => 13,
        KeyK => 14,
        KeyL => 15,
        KeyM => 16,
        KeyN => 17,
        KeyO => 18,
        KeyP => 19,
        KeyQ => 20,
        KeyR => 21,
        KeyS => 22,
        KeyT => 23,
        KeyU => 24,
        KeyV => 25,
        KeyW => 26,
        KeyX => 27,
        KeyY => 28,
        KeyZ => 29,
        Digit1 => 30,
        Digit2 => 31,
        Digit3 => 32,
        Digit4 => 33,
        Digit5 => 34,
        Digit6 => 35,
        Digit7 => 36,
        Digit8 => 37,
        Digit9 => 38,
        Digit0 => 39,
        Enter => 40,
        Escape => 41,
        Backspace => 42,
        Tab => 43,
        Space => 44,
        Minus => 45,
        Equal => 46,
        BracketLeft => 47,
        BracketRight => 48,
        Backslash => 49,
        Semicolon => 51,
        Quote => 52,
        Backquote => 53,
        Comma => 54,
        Period => 55,
        Slash => 56,
        CapsLock => 57,
        F1 => 58,
        F2 => 59,
        F3 => 60,
        F4 => 61,
        F5 => 62,
        F6 => 63,
        F7 => 64,
        F8 => 65,
        F9 => 66,
        F10 => 67,
        F11 => 68,
        F12 => 69,
        PrintScreen => 70,
        ScrollLock => 71,
        Pause => 72,
        Insert => 73,
        Home => 74,
        PageUp => 75,
        Delete => 76,
        End => 77,
        PageDown => 78,
        ArrowRight => 79,
        ArrowLeft => 80,
        ArrowDown => 81,
        ArrowUp => 82,
        NumLock => 83,
        NumpadDivide => 84,
        NumpadMultiply => 85,
        NumpadSubtract => 86,
        NumpadAdd => 87,
        NumpadEnter => 88,
        Numpad1 => 89,
        Numpad2 => 90,
        Numpad3 => 91,
        Numpad4 => 92,
        Numpad5 => 93,
        Numpad6 => 94,
        Numpad7 => 95,
        Numpad8 => 96,
        Numpad9 => 97,
        Numpad0 => 98,
        NumpadDecimal => 99,
        IntlBackslash => 100,
        ContextMenu => 101,
        ControlLeft => 224,
        ShiftLeft => 225,
        AltLeft => 226,
        SuperLeft => 227,
        ControlRight => 228,
        ShiftRight => 229,
        AltRight => 230,
        SuperRight => 231,
        _ => return None,
    })
}

/// WoW's confirmation popups ("Make this inn your home?", a resurrection, ...) as Minecraft screens
/// (2026-10-03): in Minecraft mode the shown `StaticPopup1..4` (faded, see [`FADED_FRAMES`]) goes
/// out as a [`DIALOG_CONFIRM`] window - its text and buttons - polled every few frames, and the
/// driver's pick clicks that button (closing the screen clicks the second one, WoW's Cancel). A
/// popup gone sends a close for it.
fn forward_popups(
    bridge: Res<Bridge>,
    ib: Res<InputBridge>,
    script: Option<NonSend<benilla_ui::script::UiScript>>,
    mut picks: MessageReader<benilla_app::external_dialog::DialogIn>,
    mut out: MessageWriter<benilla_app::external_dialog::DialogOut>,
    mut last: Local<String>,
    mut frames: Local<u32>,
) {
    use benilla_app::external_dialog::{DialogOut, ACT_CLOSE, ACT_SELECT, DIALOG_CLOSE, DIALOG_CONFIRM};
    let Some(script) = script else {
        picks.clear();
        return;
    };
    for d in picks.read() {
        if d.kind != DIALOG_CONFIRM || d.npc == 0 || d.npc > 4 {
            continue;
        }
        let button = match d.action {
            ACT_SELECT => d.arg + 1,
            ACT_CLOSE => 2,
            _ => continue,
        };
        let chunk = format!(
            r#"local b = getglobal("StaticPopup{}Button{}")
            if b and b:IsShown() then b:Click() return true end
            return false"#,
            d.npc, button
        );
        let clicked = script.eval::<bool>(&chunk).unwrap_or(false);
        info!("classiccraft: popup {} button {} clicked from Minecraft ({clicked})", d.npc, button);
    }
    *frames = frames.wrapping_add(1);
    if !(bridge.driving() && !ib.wow_ui) {
        last.clear();
        return;
    }
    if *frames % 6 != 0 {
        return;
    }
    // "n\x1ftext\x1fbutton1\x1fbutton2" of the first shown popup, "" for none.
    let found = script
        .eval::<String>(
            r#"for i = 1, 4 do
                local f = getglobal("StaticPopup" .. i)
                if f and f:IsShown() then
                    local t = getglobal("StaticPopup" .. i .. "Text")
                    local function label(n)
                        local b = getglobal("StaticPopup" .. i .. "Button" .. n)
                        if b and b:IsShown() then return b:GetText() or "" end
                        return ""
                    end
                    return i .. "\31" .. ((t and t:GetText()) or "") .. "\31" .. label(1) .. "\31" .. label(2)
                end
            end
            return """#,
        )
        .unwrap_or_default();
    if found == *last {
        return;
    }
    let previous = std::mem::replace(&mut *last, found.clone());
    let mut parts = found.split('\x1f');
    let Some(n) = parts.next().and_then(|n| n.parse::<u64>().ok()) else {
        // The popup went away: close its screen.
        if let Some(n) = previous.split('\x1f').next().and_then(|n| n.parse::<u64>().ok()) {
            out.write(DialogOut { npc: n, kind: DIALOG_CLOSE, ..Default::default() });
        }
        return;
    };
    let text = parts.next().unwrap_or_default().to_string();
    let options = parts.filter(|b| !b.is_empty()).map(|b| (0, b.to_string())).collect();
    info!("classiccraft: WoW popup {n} {text:?} to Minecraft");
    out.write(DialogOut { npc: n, kind: DIALOG_CONFIRM, text, options, ..Default::default() });
}
