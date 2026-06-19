#![windows_subsystem = "windows"]

use std::io::Cursor;
use std::num::NonZeroU8;
use std::time::Instant;

use include_bytes_zstd::include_bytes_zstd;
use kira::clock::ClockSpeed;
use kira::{
    manager::{backend::DefaultBackend, AudioManager, AudioManagerSettings},
    sound::streaming::{StreamingSoundData, StreamingSoundSettings},
};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::{
    core::*,
    Win32::{Foundation::*, UI::WindowsAndMessaging::*},
};

mod commandline_gui_helpers;
mod util;

const WND_CLASS: &str = "BadApple\0";

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CLOSE => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcA(hwnd, msg, wparam, lparam),
    }
}

fn register_window_class() {
    // we draw the white parts of the video, so make the background white -
    // because we use SWP_NOREDRAW, this is all we can really use to change
    // colours
    let brush = unsafe { CreateSolidBrush(COLORREF(0xFFFFFF)) };
    let icon = unsafe { LoadIconW(util::get_instance_handle(), PCWSTR(1 as _)).unwrap() };

    let wc = WNDCLASSA {
        lpfnWndProc: Some(wnd_proc),
        hInstance: util::get_instance_handle(),
        lpszClassName: PCSTR(WND_CLASS.as_ptr() as _),
        hbrBackground: brush,
        hIcon: icon,
        ..Default::default()
    };

    unsafe {
        RegisterClassA(&wc);
    };
}

struct DeferredWindow {
    hwnd: HWND,
    x: i32,
    y: i32,
    pos_stale: bool,
    w: i32,
    h: i32,
    sz_stale: bool,
    visible: bool,
    visible_stale: bool,
}

impl DeferredWindow {
    fn new_from_hwnd(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            hwnd,
            x,
            y,
            w,
            h,
            pos_stale: true,
            sz_stale: false,
            visible: true,
            visible_stale: true,
        }
    }

    fn new() -> Self {
        let w = 200;
        let h = 100;
        let x = 10;
        let y = 10;

        // takes about 1ms per window
        let hwnd = unsafe {
            CreateWindowExA(
                // WS_EX_TOOLWINDOW keeps the windows out of the taskbar.
                // WS_POPUP removes the title bar and frame entirely so each
                // window renders as a plain white rectangle.
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCSTR(WND_CLASS.as_ptr() as _),
                s!("Bad Apple!!"),
                WS_POPUP,
                // x,y,w,h
                x,
                y,
                w,
                h,
                None,
                None,
                None,
                None,
            )
        };

        assert!(hwnd.0 != 0);

        Self::new_from_hwnd(hwnd, x, y, w, h)
    }

    fn set_pos(&mut self, x: i32, y: i32) {
        self.pos_stale = self.x != x || self.y != y;
        self.x = x;
        self.y = y;
    }

    fn set_sz(&mut self, w: i32, h: i32) {
        self.sz_stale = self.w != w || self.h != h;
        self.w = w;
        self.h = h;
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible_stale = self.visible != visible;
        self.visible = visible;
    }

    fn stale(&self) -> bool {
        self.pos_stale || self.sz_stale || self.visible_stale
    }

    fn draw(&mut self, hwinposinfo: isize) -> isize {
        // SWP_NOACTIVATE: all windows stay grey
        // no SWP_NOACTIVATE: most recent window touched bounces around. Looks kinda cool.
        let mut flags = SWP_NOZORDER /*| SWP_NOACTIVATE*/;

        if !self.sz_stale {
            flags |= SWP_NOSIZE;
        }

        if !self.pos_stale {
            flags |= SWP_NOMOVE;
        }

        if self.visible_stale {
            flags |= if self.visible {
                SWP_SHOWWINDOW
            } else {
                SWP_HIDEWINDOW
            };
        }

        self.pos_stale = false;
        self.sz_stale = false;
        self.visible_stale = false;

        unsafe {
            DeferWindowPos(
                hwinposinfo,
                self.hwnd,
                None,
                self.x,
                self.y,
                self.w,
                self.h,
                flags,
            )
        }
    }
}

fn usable_monitor_sz() -> (i32, i32) {
    let mut sz: RECT = Default::default();
    assert!(unsafe {
        SystemParametersInfoA(
            SPI_GETWORKAREA,
            0,
            Some(&mut sz as *mut _ as _),
            Default::default(),
        )
        .into()
    });

    (
        (sz.right - sz.left).try_into().unwrap(),
        (sz.bottom - sz.top).try_into().unwrap(),
    )
}

struct WindowCollection {
    wins: Vec<DeferredWindow>,
}

impl WindowCollection {
    fn new(wins: Vec<DeferredWindow>) -> Self {
        Self { wins }
    }

    fn changed(&self) -> usize {
        self.wins.iter().filter(|x| x.stale()).count()
    }

    fn draw(&mut self) {
        let changed = self.changed() as i32;
        if changed == 0 {
            return;
        }

        let mut hdwp = unsafe { BeginDeferWindowPos(changed) };
        assert!(hdwp != 0);

        for win in self.wins.iter_mut().filter(|x| x.stale()) {
            hdwp = win.draw(hdwp);
            assert!(hdwp != 0);
        }

        unsafe { EndDeferWindowPos(hdwp) };
    }
}

#[derive(Debug, Copy, Clone)]
pub struct WinCoords {
    x: u8,
    y: u8,
    w: NonZeroU8,
    h: NonZeroU8,
}

// get this from `bad apple.py`
const MAX_WINDOWS: usize = 155;
const BASE_WIDTH: u8 = 64;
const BASE_HEIGHT: u8 = 48;
const TIMER_ID: usize = 1;
const FRAME_TIMER_MS: u32 = 33;

fn parse_frames(frames_raw: &[u8]) -> Vec<Option<WinCoords>> {
    let mut chunks = frames_raw.chunks_exact(4);
    let mut frames = Vec::with_capacity(chunks.len());

    for chunk in &mut chunks {
        let [x, y, w, h] = [chunk[0], chunk[1], chunk[2], chunk[3]];
        let frame = match (NonZeroU8::new(w), NonZeroU8::new(h)) {
            (None, None) => None,
            (Some(w), Some(h)) => Some(WinCoords { x, y, w, h }),
            _ => panic!("invalid frame data: width/height must both be zero or both be non-zero"),
        };
        frames.push(frame);
    }

    assert!(
        chunks.remainder().is_empty(),
        "assets/boxes.bin length must be divisible by 4"
    );

    frames
}

fn main() {
    commandline_gui_helpers::init();

    register_window_class();

    let frames_raw = include_bytes_zstd!("assets/boxes.bin", 22);
    let frames = parse_frames(&frames_raw);
    let mut frames_iter = frames.iter();
    // println!("{:?}", frames);

    unsafe {
        // todo WM_PARENTNOTIFY

        println!("Creating windows...");
        let now = Instant::now();
        let wins: Vec<DeferredWindow> = (0..MAX_WINDOWS).map(|_| DeferredWindow::new()).collect();
        println!("Done! in {:?}", now.elapsed());

        let mut collection = WindowCollection::new(wins);

        // Audio playback
        let cursor = Cursor::new(include_bytes!("../assets/bad apple.ogg"));
        let mut manager =
            AudioManager::<DefaultBackend>::new(AudioManagerSettings::default()).unwrap();
        let clock = manager
            .add_clock(ClockSpeed::TicksPerSecond(30f64))
            .unwrap();
        let mut next_tick = clock.time().ticks;
        let sound_data = StreamingSoundData::from_cursor(
            cursor,
            StreamingSoundSettings::new().start_time(clock.time()),
        )
        .unwrap();
        manager.play(sound_data).unwrap();
        clock.start().unwrap();

        // println!("Showing windows...");
        // let now = Instant::now();
        // Normal windows (appear in taskbar and alt-tab):
        //   ~15ms per window for 100
        //   ~44ms per window for 500
        // WS_EX_TOOLWINDOW:
        //   ~7ms  per window for 100
        //   ~15ms per window for 500
        // wins.iter().for_each(|win| {ShowWindow(*win, SW_SHOW);});
        // println!("Done! in {:?}", now.elapsed());

        let (usable_x, usable_y) = usable_monitor_sz();
        let ratio_x = usable_x as f32 / BASE_WIDTH as f32;
        let ratio_y = usable_y as f32 / BASE_HEIGHT as f32;

        let timer = SetTimer(None, TIMER_ID, FRAME_TIMER_MS, None);
        assert!(timer != 0, "failed to create frame timer");

        let mut reached_end = false;

        loop {
            let mut msg: MSG = std::mem::zeroed();
            let status = GetMessageA(&mut msg, None, 0, 0).0;
            assert!(status != -1, "GetMessageA failed");

            if status == 0 {
                println!("WM_QUIT");
                break;
            }

            if msg.message == WM_TIMER {
                let current_tick = clock.time().ticks;
                // nothing to do yet, eg next_tick = 1, current_tick = 0
                if next_tick > current_tick {
                    continue;
                }

                // skip any frames that we missed, eg next_tick = 2, current_tick = 3
                while current_tick > next_tick {
                    loop {
                        let Some(val) = frames_iter.next() else {
                            reached_end = true;
                            break;
                        };
                        let Some(_coords) = val else {
                            break;
                        };
                    }

                    if reached_end {
                        break;
                    }

                    next_tick += 1;
                }

                if reached_end {
                    break;
                }

                // process the current tick
                let mut windows = collection.wins.iter_mut();
                loop {
                    let Some(val) = frames_iter.next() else {
                        reached_end = true;
                        break;
                    };
                    let Some(coords) = val else {
                        break;
                    };

                    let win = windows.next().unwrap();
                    win.set_pos(
                        (coords.x as f32 * ratio_x) as i32,
                        (coords.y as f32 * ratio_y) as i32,
                    );
                    win.set_sz(
                        (coords.w.get() as f32 * ratio_x) as i32,
                        (coords.h.get() as f32 * ratio_y) as i32,
                    );
                    win.set_visible(true);
                }

                if reached_end {
                    break;
                }

                // hide the rest
                for win in windows {
                    win.set_visible(false);
                }

                collection.draw();
                next_tick += 1;
            }

            TranslateMessage(&msg);
            DispatchMessageA(&msg);
        }

        KillTimer(None, TIMER_ID);
        for win in &collection.wins {
            DestroyWindow(win.hwnd);
        }
    }
}
