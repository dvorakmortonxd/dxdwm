use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ButtonIndex, ButtonPressEvent, ButtonReleaseEvent, ChangeWindowAttributesAux,
    ClientMessageData, ClientMessageEvent, ConfigWindow, ConfigureRequestEvent,
    ConfigureWindowAux, ConnectionExt, EventMask, GrabMode, InputFocus, KeyPressEvent,
    MapRequestEvent, ModMask, MotionNotifyEvent, StackMode, UnmapNotifyEvent, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

const KEYSYM_SUPER_L: u32 = 0xFFEB;
const KEYSYM_SUPER_R: u32 = 0xFFEC;
const KEYSYM_RETURN: u32 = 0xFF0D;
const KEYSYM_Q: u32 = 0x0071;
const KEYSYM_W: u32 = 0x0077;
const KEYSYM_SPACE: u32 = 0x0020;
const KEYSYM_NUM_LOCK: u32 = 0xFF7F;
const MIN_WIN_SIZE: u16 = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutMode {
    Floating,
    Tabbed,
}

#[derive(Clone, Copy)]
enum DragMode {
    Move,
    Resize,
}

#[derive(Clone, Copy)]
struct DragState {
    window: Window,
    mode: DragMode,
    start_root_x: i16,
    start_root_y: i16,
    start_x: i32,
    start_y: i32,
    start_width: u16,
    start_height: u16,
}

struct Wm {
    conn: RustConnection,
    root: Window,
    managed: HashSet<Window>,
    super_keycodes: Vec<u8>,
    terminal_keycodes: Vec<u8>,
    close_keycodes: Vec<u8>,
    cycle_keycodes: Vec<u8>,
    toggle_mode_keycodes: Vec<u8>,
    num_lock_mask: u16,
    managed_order: Vec<Window>,
    focused: Option<Window>,
    layout_mode: LayoutMode,
    drag: Option<DragState>,
}

impl Wm {
    fn new() -> Result<Self, Box<dyn Error>> {
        let display = env::var("DISPLAY")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "unset".to_string());

        let (conn, screen_num) = x11rb::connect(None).map_err(|err| {
            io::Error::other(format!(
                "Failed to connect to X server using DISPLAY={display}. Original error: {err}"
            ))
        })?;
        let root = conn.setup().roots[screen_num].root;

        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::default().event_mask(
                EventMask::SUBSTRUCTURE_REDIRECT
                    | EventMask::SUBSTRUCTURE_NOTIFY
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::STRUCTURE_NOTIFY,
            ),
        )?
        .check()
        .map_err(|_| {
            "Unable to become window manager. Another WM probably owns SubstructureRedirect."
        })?;

        let mut wm = Self {
            conn,
            root,
            managed: HashSet::new(),
            super_keycodes: Vec::new(),
            terminal_keycodes: Vec::new(),
            close_keycodes: Vec::new(),
            cycle_keycodes: Vec::new(),
            toggle_mode_keycodes: Vec::new(),
            num_lock_mask: 0,
            managed_order: Vec::new(),
            focused: None,
            layout_mode: LayoutMode::Floating,
            drag: None,
        };

        wm.setup_input_grabs()?;
        wm.take_existing_windows()?;
        wm.advertise_wm_selection()?;
        wm.conn.flush()?;

        Ok(wm)
    }

    fn run(&mut self) -> Result<(), Box<dyn Error>> {
        loop {
            let event = self.conn.wait_for_event()?;
            match event {
                x11rb::protocol::Event::MapRequest(e) => self.on_map_request(e)?,
                x11rb::protocol::Event::UnmapNotify(e) => self.on_unmap(e),
                x11rb::protocol::Event::DestroyNotify(e) => self.on_unmanage(e.window),
                x11rb::protocol::Event::ConfigureRequest(e) => self.on_configure_request(e)?,
                x11rb::protocol::Event::KeyPress(e) => self.on_key_press(e)?,
                x11rb::protocol::Event::ButtonPress(e) => self.on_button_press(e)?,
                x11rb::protocol::Event::MotionNotify(e) => self.on_motion(e)?,
                x11rb::protocol::Event::ButtonRelease(e) => self.on_button_release(e)?,
                _ => {}
            }
            self.conn.flush()?;
        }
    }

    fn advertise_wm_selection(&self) -> Result<(), Box<dyn Error>> {
        // WM_S0 is the canonical selection atom for screen 0.
        let atom = self
            .conn
            .intern_atom(false, b"WM_S0")?
            .reply()?
            .atom;
        let _ = self.conn.set_selection_owner(self.root, atom, CURRENT_TIME)?;
        Ok(())
    }

    fn take_existing_windows(&mut self) -> Result<(), Box<dyn Error>> {
        let children = self.conn.query_tree(self.root)?.reply()?.children;
        for win in children {
            let attrs = self.conn.get_window_attributes(win)?.reply()?;
            if attrs.override_redirect {
                continue;
            }
            self.manage_window(win)?;
        }
        Ok(())
    }

    fn setup_input_grabs(&mut self) -> Result<(), Box<dyn Error>> {
        self.num_lock_mask = self.detect_num_lock_mask()?;

        self.super_keycodes = self.keycodes_for_keysyms(&[KEYSYM_SUPER_L, KEYSYM_SUPER_R])?;
        self.terminal_keycodes = self.keycodes_for_keysyms(&[KEYSYM_RETURN])?;
        self.close_keycodes = self.keycodes_for_keysyms(&[KEYSYM_Q])?;
        self.cycle_keycodes = self.keycodes_for_keysyms(&[KEYSYM_W])?;
        self.toggle_mode_keycodes = self.keycodes_for_keysyms(&[KEYSYM_SPACE])?;

        for keycode in &self.super_keycodes {
            for mods in self.modifier_variants(ModMask::from(0u16)) {
                self.conn.grab_key(
                    false,
                    self.root,
                    mods,
                    *keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?;
            }
        }

        for keycode in &self.terminal_keycodes {
            for mods in self.modifier_variants(ModMask::M1) {
                self.conn.grab_key(
                    false,
                    self.root,
                    mods,
                    *keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?;
            }
        }

        for keycode in &self.close_keycodes {
            for mods in self.modifier_variants(ModMask::M1) {
                self.conn.grab_key(
                    false,
                    self.root,
                    mods,
                    *keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?;
            }
        }

        for keycode in &self.cycle_keycodes {
            for mods in self.modifier_variants(ModMask::M1) {
                self.conn.grab_key(
                    false,
                    self.root,
                    mods,
                    *keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?;
            }
        }

        for keycode in &self.toggle_mode_keycodes {
            for mods in self.modifier_variants(ModMask::M1) {
                self.conn.grab_key(
                    false,
                    self.root,
                    mods,
                    *keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?;
            }
        }

        Ok(())
    }

    fn keycodes_for_keysyms(&self, keysyms: &[u32]) -> Result<Vec<u8>, Box<dyn Error>> {
        let mapping = self.conn.get_keyboard_mapping(8, 248)?.reply()?;
        let mut keycodes = Vec::new();

        for (idx, syms) in mapping
            .keysyms
            .chunks(mapping.keysyms_per_keycode as usize)
            .enumerate()
        {
            if syms.iter().any(|s| keysyms.contains(s)) {
                keycodes.push((idx + 8) as u8);
            }
        }

        Ok(keycodes)
    }

    fn detect_num_lock_mask(&self) -> Result<u16, Box<dyn Error>> {
        let modifier_map = self.conn.get_modifier_mapping()?.reply()?;
        let keycodes_per_modifier = modifier_map.keycodes_per_modifier() as usize;
        let modifier_masks = [
            u16::from(ModMask::SHIFT),
            u16::from(ModMask::LOCK),
            u16::from(ModMask::CONTROL),
            u16::from(ModMask::M1),
            u16::from(ModMask::M2),
            u16::from(ModMask::M3),
            u16::from(ModMask::M4),
            u16::from(ModMask::M5),
        ];

        for (idx, mask_bits) in modifier_masks.iter().copied().enumerate() {
            let start = idx * keycodes_per_modifier;
            let end = start + keycodes_per_modifier;
            for keycode in &modifier_map.keycodes[start..end] {
                if *keycode == 0 {
                    continue;
                }

                let reply = self.conn.get_keyboard_mapping(*keycode, 1)?.reply()?;
                if reply.keysyms.contains(&KEYSYM_NUM_LOCK) {
                    return Ok(mask_bits);
                }
            }
        }

        Ok(0)
    }

    fn modifier_variants(&self, base: ModMask) -> Vec<ModMask> {
        let base_bits = u16::from(base);
        let lock_bits = u16::from(ModMask::LOCK);
        let mut variants = vec![
            ModMask::from(base_bits),
            ModMask::from(base_bits | lock_bits),
        ];
        if self.num_lock_mask != 0 {
            variants.push(ModMask::from(base_bits | self.num_lock_mask));
            variants.push(ModMask::from(base_bits | self.num_lock_mask | lock_bits));
        }
        variants.sort_by_key(|mask| u16::from(*mask));
        variants.dedup_by_key(|mask| u16::from(*mask));
        variants
    }

    fn normalize_modifiers(&self, state_bits: u16) -> u16 {
        let mut bits = state_bits;
        bits &= !u16::from(ModMask::LOCK);
        bits &= !self.num_lock_mask;
        bits
    }

    fn on_map_request(&mut self, e: MapRequestEvent) -> Result<(), Box<dyn Error>> {
        self.conn.map_window(e.window)?;
        self.manage_window(e.window)?;
        self.focus(e.window)?;
        self.apply_layout()?;
        Ok(())
    }

    fn on_unmap(&mut self, e: UnmapNotifyEvent) {
        if e.event != self.root {
            self.on_unmanage(e.window);
        }
    }

    fn on_unmanage(&mut self, window: Window) {
        self.managed.remove(&window);
        self.managed_order.retain(|w| *w != window);
        if self.focused == Some(window) {
            self.focused = self.managed_order.last().copied();
        }
        if matches!(self.drag, Some(d) if d.window == window) {
            self.drag = None;
        }

        let _ = self.apply_layout();
    }

    fn on_configure_request(&self, e: ConfigureRequestEvent) -> Result<(), Box<dyn Error>> {
        // This WM is floating by default, so client configure requests are honored.
        let mut aux = ConfigureWindowAux::default();
        if e.value_mask.contains(ConfigWindow::X) {
            aux = aux.x(i32::from(e.x));
        }
        if e.value_mask.contains(ConfigWindow::Y) {
            aux = aux.y(i32::from(e.y));
        }
        if e.value_mask.contains(ConfigWindow::WIDTH) {
            aux = aux.width(u32::from(e.width));
        }
        if e.value_mask.contains(ConfigWindow::HEIGHT) {
            aux = aux.height(u32::from(e.height));
        }
        if e.value_mask.contains(ConfigWindow::BORDER_WIDTH) {
            aux = aux.border_width(u32::from(e.border_width));
        }
        if e.value_mask.contains(ConfigWindow::SIBLING) {
            aux = aux.sibling(e.sibling);
        }
        if e.value_mask.contains(ConfigWindow::STACK_MODE) {
            aux = aux.stack_mode(e.stack_mode);
        }
        self.conn.configure_window(e.window, &aux)?;
        Ok(())
    }

    fn on_key_press(&mut self, e: KeyPressEvent) -> Result<(), Box<dyn Error>> {
        let state_bits = self.normalize_modifiers(u16::from(e.state));

        if self.terminal_keycodes.contains(&e.detail)
            && (state_bits & u16::from(ModMask::M1)) != 0
        {
            if let Err(err) = spawn_terminal() {
                eprintln!("failed to launch terminal: {err}");
            }
            return Ok(());
        }

        if self.close_keycodes.contains(&e.detail) && (state_bits & u16::from(ModMask::M1)) != 0 {
            self.close_focused_window()?;
            return Ok(());
        }

        if self.cycle_keycodes.contains(&e.detail) && (state_bits & u16::from(ModMask::M1)) != 0 {
            self.focus_next_window()?;
            return Ok(());
        }

        if self.toggle_mode_keycodes.contains(&e.detail)
            && (state_bits & u16::from(ModMask::M1)) != 0
        {
            self.toggle_layout_mode()?;
            return Ok(());
        }

        if self.super_keycodes.contains(&e.detail) && state_bits == 0 {
            if let Err(err) = spawn_rofi() {
                eprintln!("failed to launch rofi: {err}");
            }
        }

        Ok(())
    }

    fn on_button_press(&mut self, e: ButtonPressEvent) -> Result<(), Box<dyn Error>> {
        if self.layout_mode == LayoutMode::Tabbed {
            return Ok(());
        }

        let is_alt = u16::from(e.state) & u16::from(ModMask::M1) != 0;
        if !is_alt {
            return Ok(());
        }

        if !self.managed.contains(&e.event) {
            return Ok(());
        }

        let geom = self.conn.get_geometry(e.event)?.reply()?;
        let mode = match e.detail {
            x if x == ButtonIndex::M1.into() => DragMode::Move,
            x if x == ButtonIndex::M3.into() => DragMode::Resize,
            _ => return Ok(()),
        };

        self.focus(e.event)?;

        self.conn.grab_pointer(
            false,
            self.root,
            EventMask::BUTTON_RELEASE | EventMask::BUTTON_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            NONE,
            NONE,
            CURRENT_TIME,
        )?;

        self.drag = Some(DragState {
            window: e.event,
            mode,
            start_root_x: e.root_x,
            start_root_y: e.root_y,
            start_x: i32::from(geom.x),
            start_y: i32::from(geom.y),
            start_width: geom.width,
            start_height: geom.height,
        });

        Ok(())
    }

    fn on_motion(&mut self, e: MotionNotifyEvent) -> Result<(), Box<dyn Error>> {
        let Some(drag) = self.drag else {
            return Ok(());
        };

        let dx = i32::from(e.root_x) - i32::from(drag.start_root_x);
        let dy = i32::from(e.root_y) - i32::from(drag.start_root_y);

        let aux = match drag.mode {
            DragMode::Move => ConfigureWindowAux::default().x(drag.start_x + dx).y(drag.start_y + dy),
            DragMode::Resize => {
                let w = (i32::from(drag.start_width) + dx).max(i32::from(MIN_WIN_SIZE)) as u32;
                let h = (i32::from(drag.start_height) + dy).max(i32::from(MIN_WIN_SIZE)) as u32;
                ConfigureWindowAux::default().width(w).height(h)
            }
        };

        self.conn.configure_window(drag.window, &aux)?;
        Ok(())
    }

    fn on_button_release(&mut self, e: ButtonReleaseEvent) -> Result<(), Box<dyn Error>> {
        let should_stop = match (self.drag, e.detail) {
            (Some(DragState { mode: DragMode::Move, .. }), d) if d == ButtonIndex::M1.into() => {
                true
            }
            (Some(DragState { mode: DragMode::Resize, .. }), d)
                if d == ButtonIndex::M3.into() =>
            {
                true
            }
            _ => false,
        };

        if should_stop {
            self.drag = None;
            self.conn.ungrab_pointer(CURRENT_TIME)?;
        }

        Ok(())
    }

    fn manage_window(&mut self, window: Window) -> Result<(), Box<dyn Error>> {
        if self.managed.contains(&window) {
            return Ok(());
        }

        let attrs = self.conn.get_window_attributes(window)?.reply()?;
        if attrs.override_redirect {
            return Ok(());
        }

        // Passive grabs allow Alt+Button drag even when focus is elsewhere.
        for mods in self.modifier_variants(ModMask::M1) {
            self.conn.grab_button(
                false,
                window,
                EventMask::BUTTON_PRESS,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                NONE,
                ButtonIndex::M1,
                mods,
            )?;
            self.conn.grab_button(
                false,
                window,
                EventMask::BUTTON_PRESS,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                NONE,
                ButtonIndex::M3,
                mods,
            )?;
        }

        self.managed.insert(window);
        if !self.managed_order.contains(&window) {
            self.managed_order.push(window);
        }
        Ok(())
    }

    fn focus(&mut self, window: Window) -> Result<(), Box<dyn Error>> {
        if let Some(pos) = self.managed_order.iter().position(|w| *w == window) {
            let w = self.managed_order.remove(pos);
            self.managed_order.push(w);
        }
        self.focused = Some(window);

        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, window, CURRENT_TIME)?;
        self.conn.configure_window(
            window,
            &ConfigureWindowAux::default().stack_mode(StackMode::ABOVE),
        )?;
        Ok(())
    }

    fn close_focused_window(&self) -> Result<(), Box<dyn Error>> {
        let Some(window) = self.focused else {
            return Ok(());
        };

        let wm_protocols = self
            .conn
            .intern_atom(false, b"WM_PROTOCOLS")?
            .reply()?
            .atom;
        let wm_delete_window = self
            .conn
            .intern_atom(false, b"WM_DELETE_WINDOW")?
            .reply()?
            .atom;

        let protocols = self
            .conn
            .get_property(false, window, wm_protocols, x11rb::NONE, 0, u32::MAX)?
            .reply()?;

        let supports_delete = protocols
            .value32()
            .map(|atoms| atoms.into_iter().any(|atom| atom == wm_delete_window))
            .unwrap_or(false);

        if supports_delete {
            let data = ClientMessageData::from([wm_delete_window, CURRENT_TIME, 0, 0, 0]);
            let event = ClientMessageEvent::new(32, window, wm_protocols, data);
            self.conn.send_event(false, window, EventMask::NO_EVENT, event)?;
        } else {
            self.conn.kill_client(window)?;
        }

        Ok(())
    }

    fn focus_next_window(&mut self) -> Result<(), Box<dyn Error>> {
        if self.managed_order.is_empty() {
            return Ok(());
        }

        let next_window = match self.focused {
            Some(current) => {
                let idx = self
                    .managed_order
                    .iter()
                    .position(|w| *w == current)
                    .unwrap_or(0);
                let next_idx = (idx + 1) % self.managed_order.len();
                self.managed_order[next_idx]
            }
            None => self.managed_order[0],
        };

        self.focus(next_window)?;
        self.apply_layout()?;
        Ok(())
    }

    fn toggle_layout_mode(&mut self) -> Result<(), Box<dyn Error>> {
        self.layout_mode = match self.layout_mode {
            LayoutMode::Floating => LayoutMode::Tabbed,
            LayoutMode::Tabbed => LayoutMode::Floating,
        };
        self.apply_layout()?;
        Ok(())
    }

    fn apply_layout(&mut self) -> Result<(), Box<dyn Error>> {
        if self.managed_order.is_empty() {
            self.focused = None;
            return Ok(());
        }

        if self.focused.is_none() {
            self.focused = self.managed_order.last().copied();
        }

        match self.layout_mode {
            LayoutMode::Floating => {
                for window in &self.managed_order {
                    let _ = self.conn.map_window(*window);
                }
            }
            LayoutMode::Tabbed => {
                let focused = self.focused.unwrap_or(self.managed_order[0]);
                let root_geom = self.conn.get_geometry(self.root)?.reply()?;

                for window in &self.managed_order {
                    if *window == focused {
                        let _ = self.conn.map_window(*window);
                        self.conn.configure_window(
                            *window,
                            &ConfigureWindowAux::default()
                                .x(0)
                                .y(0)
                                .width(u32::from(root_geom.width))
                                .height(u32::from(root_geom.height)),
                        )?;
                    } else {
                        let _ = self.conn.unmap_window(*window);
                    }
                }

                self.focus(focused)?;
            }
        }

        Ok(())
    }
}

fn spawn_rofi() -> Result<(), Box<dyn Error>> {
    let launcher = env::var("DXDWM_LAUNCHER").unwrap_or_else(|_| "rofi -show drun".to_string());
    Command::new("sh")
        .arg("-c")
        .arg(launcher)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn spawn_terminal() -> Result<(), Box<dyn Error>> {
    let terminal = env::var("DXDWM_TERMINAL").unwrap_or_else(|_| "alacritty".to_string());
    Command::new("sh")
        .arg("-c")
        .arg(terminal)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("dxdwm exited with error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    ensure_display_set()?;
    ensure_and_run_startup_script()?;
    let mut wm = Wm::new()?;
    wm.run()
}

fn ensure_and_run_startup_script() -> Result<(), Box<dyn Error>> {
    let home = env::var("HOME").map_err(|_| io::Error::other("HOME is not set"))?;
    let config_dir = PathBuf::from(home).join(".config/dxdwm");
    let script_path = config_dir.join("dxdwm.sh");

    fs::create_dir_all(&config_dir)?;

    if !script_path.exists() {
        fs::write(
            &script_path,
            "#!/usr/bin/env sh\n# dxdwm startup hook\n# Add your startup apps here, one per line.\n\n",
        )?;
    }

    #[cfg(unix)]
    {
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&script_path, perms)?;
    }

    Command::new("sh")
        .arg(script_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    Ok(())
}

fn ensure_display_set() -> Result<(), Box<dyn Error>> {
    let display_is_set = env::var("DISPLAY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);

    if !display_is_set {
        return Err(io::Error::other(
            "DISPLAY is not set. Start this inside an X11 session or use scripts/run_xephyr.sh from an existing X session.",
        )
        .into());
    }

    Ok(())
}


