use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::io;
use std::process::{Command, Stdio};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ButtonIndex, ButtonPressEvent, ButtonReleaseEvent, ChangeWindowAttributesAux,
    ConfigWindow, ConfigureRequestEvent, ConfigureWindowAux, ConnectionExt, EventMask,
    GrabMode, InputFocus, KeyPressEvent, MapRequestEvent, ModMask, MotionNotifyEvent, StackMode,
    UnmapNotifyEvent, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::{CURRENT_TIME, NONE};

const KEYSYM_SUPER_L: u32 = 0xFFEB;
const KEYSYM_SUPER_R: u32 = 0xFFEC;
const MIN_WIN_SIZE: u16 = 64;

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
            drag: None,
        };

        wm.grab_super_key()?;
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
                x11rb::protocol::Event::KeyPress(e) => self.on_key_press(e),
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

    fn grab_super_key(&mut self) -> Result<(), Box<dyn Error>> {
        let mapping = self.conn.get_keyboard_mapping(8, 248)?.reply()?;

        for (idx, syms) in mapping
            .keysyms
            .chunks(mapping.keysyms_per_keycode as usize)
            .enumerate()
        {
            if syms
                .iter()
                .any(|s| *s == KEYSYM_SUPER_L || *s == KEYSYM_SUPER_R)
            {
                let keycode = (idx + 8) as u8;
                self.super_keycodes.push(keycode);

                for mods in [
                    ModMask::M4,
                    ModMask::M4 | ModMask::LOCK,
                    ModMask::M4 | ModMask::M2,
                    ModMask::M4 | ModMask::M2 | ModMask::LOCK,
                ] {
                    self.conn.grab_key(
                        false,
                        self.root,
                        mods,
                        keycode,
                        GrabMode::ASYNC,
                        GrabMode::ASYNC,
                    )?;
                }
            }
        }

        Ok(())
    }

    fn on_map_request(&mut self, e: MapRequestEvent) -> Result<(), Box<dyn Error>> {
        self.conn.map_window(e.window)?;
        self.manage_window(e.window)?;
        self.focus(e.window)?;
        Ok(())
    }

    fn on_unmap(&mut self, e: UnmapNotifyEvent) {
        if e.event != self.root {
            self.on_unmanage(e.window);
        }
    }

    fn on_unmanage(&mut self, window: Window) {
        self.managed.remove(&window);
        if matches!(self.drag, Some(d) if d.window == window) {
            self.drag = None;
        }
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

    fn on_key_press(&self, e: KeyPressEvent) {
        if self.super_keycodes.contains(&e.detail) {
            if let Err(err) = spawn_rofi() {
                eprintln!("failed to launch rofi: {err}");
            }
        }
    }

    fn on_button_press(&mut self, e: ButtonPressEvent) -> Result<(), Box<dyn Error>> {
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
        for mods in [
            ModMask::M1,
            ModMask::M1 | ModMask::LOCK,
            ModMask::M1 | ModMask::M2,
            ModMask::M1 | ModMask::M2 | ModMask::LOCK,
        ] {
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
        Ok(())
    }

    fn focus(&self, window: Window) -> Result<(), Box<dyn Error>> {
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, window, CURRENT_TIME)?;
        self.conn.configure_window(
            window,
            &ConfigureWindowAux::default().stack_mode(StackMode::ABOVE),
        )?;
        Ok(())
    }
}

fn spawn_rofi() -> Result<(), Box<dyn Error>> {
    Command::new("sh")
        .arg("-c")
        .arg("rofi -show drun")
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
    let mut wm = Wm::new()?;
    wm.run()
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


