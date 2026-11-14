//! Küresel kısayol, Raw Input / HID (çok tuşlu MMO fareleri dahil) yakalama,
//! Passthrough filtreleme ve Push-to-Talk (`SendInput`) modülü.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidP_GetUsagesEx, HidP_Input, PHIDP_PREPARSED_DATA, USAGE_AND_PAGE,
};
use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList, RegisterRawInputDevices, HRAWINPUT,
    RAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICELIST, RAWINPUTHEADER, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK,
    RIDEV_PAGEONLY, RIDI_DEVICEINFO, RIDI_PREPARSEDDATA, RID_DEVICE_INFO, RID_INPUT, RIM_TYPEHID,
    RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    SetWindowsHookExW, TranslateMessage, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP, MSG, MSLLHOOKSTRUCT,
    RI_MOUSE_BUTTON_3_DOWN, RI_MOUSE_BUTTON_3_UP, RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP,
    RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_INPUT,
    WM_INPUT_DEVICE_CHANGE, WM_KEYDOWN, WM_KEYUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
};

use crate::commands::AppState;
use crate::library::HotkeyBinding;

/// Kendi `SendInput` çağrılarımızı ayırt etmek için kullanılan imza.
const OZAN_SYNTHETIC_TAG: usize = 0x02A4_F00D;

const XBUTTON1_HI: u16 = 1;
const XBUTTON2_HI: u16 = 2;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedKey {
    pub code: String,
    pub label: String,
}

#[derive(Default)]
struct InputTable {
    /// code -> Vec<(sound_id, passthrough)>
    sound_bindings: HashMap<String, Vec<(String, bool)>>,
    panic: Option<HotkeyBinding>,
    ptt_enabled: bool,
    ptt_hotkey: Option<HotkeyBinding>,
}

struct InputRuntime {
    app: AppHandle,
    table: Mutex<InputTable>,
    /// Şu anda basılı olan tuş kodları (basılı tutarken otomatik tekrarı önlemek için).
    pressed_codes: Mutex<HashSet<String>>,
    /// Raw Input ile LL Hook arasında çift tetiklemeyi önlemek için son tetikleme zamanı.
    recent_fires: Mutex<HashMap<String, Instant>>,
    /// HID cihaz başına önceki basılı kullanım listesi ve ham rapor durumu.
    hid_pressed: Mutex<HashMap<isize, HashSet<(u16, u16)>>>,
    hid_raw_prev: Mutex<HashMap<isize, Vec<u8>>>,
    /// `ulRawButtons` önceki bit maskesi.
    raw_mouse_buttons: Mutex<u32>,
    /// Kısayol öğrenme modu aktif mi?
    capturing: AtomicBool,
    /// PTT tuşu şu anda SendInput ile basılı mı?
    ptt_currently_down: AtomicBool,
    ptt_held_code: Mutex<Option<String>>,
}

static RUNTIME: OnceLock<InputRuntime> = OnceLock::new();

pub fn init(app: AppHandle) {
    let rt = InputRuntime {
        app,
        table: Mutex::new(InputTable::default()),
        pressed_codes: Mutex::new(HashSet::new()),
        recent_fires: Mutex::new(HashMap::new()),
        hid_pressed: Mutex::new(HashMap::new()),
        hid_raw_prev: Mutex::new(HashMap::new()),
        raw_mouse_buttons: Mutex::new(0),
        capturing: AtomicBool::new(false),
        ptt_currently_down: AtomicBool::new(false),
        ptt_held_code: Mutex::new(None),
    };
    let _ = RUNTIME.set(rt);
    sync_from_library();

    std::thread::Builder::new()
        .name("ozan-input".into())
        .spawn(|| unsafe { input_thread_main() })
        .ok();
}

/// Kütüphanedeki kısayolları, Panic tuşunu ve PTT ayarlarını bellekteki tabloya senkronize eder.
pub fn sync_from_library() {
    let Some(rt) = RUNTIME.get() else { return };
    let Some(state) = rt.app.try_state::<AppState>() else { return };
    let lib = state.lib.lock().unwrap_or_else(|p| p.into_inner());
    let mut table = rt.table.lock().unwrap_or_else(|p| p.into_inner());
    table.sound_bindings.clear();
    for s in &lib.data.sounds {
        if let Some(hk) = &s.hotkey {
            if !hk.code.is_empty() {
                table
                    .sound_bindings
                    .entry(hk.code.clone())
                    .or_default()
                    .push((s.id.clone(), hk.passthrough));
            }
        }
    }
    table.panic = Some(lib.data.settings.panic_hotkey.clone());
    table.ptt_enabled = lib.data.settings.ptt_enabled;
    table.ptt_hotkey = lib.data.settings.ptt_hotkey.clone();
}

/// Arayüzde kullanıcı "Tuş Ata" dediğinde bir sonraki tuş/fare/MMO butonunu yakalama modunu açar.
pub fn start_capture() {
    if let Some(rt) = RUNTIME.get() {
        rt.capturing.store(true, Ordering::SeqCst);
    }
}

pub fn cancel_capture() {
    if let Some(rt) = RUNTIME.get() {
        rt.capturing.store(false, Ordering::SeqCst);
    }
}

/// Ses çalmaya başladığında (`hold = true`) veya tüm sesler bittiğinde (`hold = false`)
/// arka planda `SendInput` ile Bas-Konuş (Push-to-Talk) tuşuna basılı tutar / bırakır.
pub fn set_ptt_held(hold: bool) {
    let Some(rt) = RUNTIME.get() else { return };
    if hold {
        let (enabled, hk) = {
            let t = rt.table.lock().unwrap_or_else(|p| p.into_inner());
            (t.ptt_enabled, t.ptt_hotkey.clone())
        };
        if !enabled {
            return;
        }
        let Some(hk) = hk else { return };
        if hk.code.is_empty() {
            return;
        }
        if !rt.ptt_currently_down.swap(true, Ordering::SeqCst) {
            if let Ok(mut g) = rt.ptt_held_code.lock() {
                *g = Some(hk.code.clone());
            }
            send_synthetic_input(&hk.code, true);
        }
    } else if rt.ptt_currently_down.swap(false, Ordering::SeqCst) {
        let code = rt.ptt_held_code.lock().ok().and_then(|mut g| g.take());
        if let Some(c) = code {
            send_synthetic_input(&c, false);
        }
    }
}

/// `SendInput` kullanarak verilen tuş/fare koduna basar (`down = true`) veya bırakır (`down = false`).
fn send_synthetic_input(code: &str, down: bool) {
    unsafe {
        if let Some(vk_str) = code.strip_prefix("VK:") {
            let parts: Vec<&str> = vk_str.split(':').collect();
            let Ok(vk) = parts[0].parse::<u16>() else { return };
            let ext = parts.get(1).copied() == Some("E");
            let scan = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16;
            let mut flags = if scan != 0 { KEYEVENTF_SCANCODE } else { 0 };
            if ext {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: OZAN_SYNTHETIC_TAG,
                    },
                },
            };
            SendInput(1, &input, size_of::<INPUT>() as i32);
        } else if code == "MOUSE:X1" || code == "MOUSE:X2" {
            let xbtn = if code == "MOUSE:X1" { XBUTTON1_HI as u32 } else { XBUTTON2_HI as u32 };
            let flags = if down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP };
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: xbtn,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: OZAN_SYNTHETIC_TAG,
                    },
                },
            };
            SendInput(1, &input, size_of::<INPUT>() as i32);
        } else if code == "MOUSE:M3" {
            let flags = if down { MOUSEEVENTF_MIDDLEDOWN } else { MOUSEEVENTF_MIDDLEUP };
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: OZAN_SYNTHETIC_TAG,
                    },
                },
            };
            SendInput(1, &input, size_of::<INPUT>() as i32);
        }
    }
}

/// Bir tuş/buton olayı geldiğinde çağrılır.
/// Dönüş değeri: `true` ise olay aktif oyuna/uygulamaya İLETİLMEMELİ (suppress), `false` ise iletilmeli (passthrough).
fn handle_button_event(code: String, label: String, is_down: bool) -> bool {
    let Some(rt) = RUNTIME.get() else { return false };

    if !is_down {
        if let Ok(mut pressed) = rt.pressed_codes.lock() {
            pressed.remove(&code);
        }
        return false;
    }

    // Kısayol öğrenme (atama) modundaysak basılan ilk tuşu yakala ve arayüze bildir.
    if rt.capturing.swap(false, Ordering::SeqCst) {
        let _ = rt.app.emit("hotkey-captured", CapturedKey { code, label });
        return true; // yakalama sırasında oyuna gitmesin
    }

    // Basılı tutulan tuşun otomatik tekrarını (auto-repeat) yok say.
    {
        let mut pressed = rt.pressed_codes.lock().unwrap_or_else(|p| p.into_inner());
        if !pressed.insert(code.clone()) {
            // Zaten basılı; eğer passthrough kapalı bir kısayolsa yine de suppress etmeye devam et.
            return should_suppress(&rt.table, &code);
        }
    }

    // LL Hook + Raw Input aynı tuşu birkaç milisaniye arayla bildirebilir; çift tetiklemeyi önle.
    {
        let now = Instant::now();
        let mut recent = rt.recent_fires.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(&prev) = recent.get(&code) {
            if now.duration_since(prev) < Duration::from_millis(35) {
                return should_suppress(&rt.table, &code);
            }
        }
        recent.insert(code.clone(), now);
    }

    let (is_panic, panic_passthrough, matched_sounds) = {
        let t = rt.table.lock().unwrap_or_else(|p| p.into_inner());
        let (is_p, p_pass) = match &t.panic {
            Some(p) if p.code == code => (true, p.passthrough),
            _ => (false, true),
        };
        let sounds = t.sound_bindings.get(&code).cloned().unwrap_or_default();
        (is_p, p_pass, sounds)
    };

    if is_panic {
        if let Some(state) = rt.app.try_state::<AppState>() {
            state.engine.stop_all();
        }
        let _ = rt.app.emit("panic-triggered", ());
        return !panic_passthrough;
    }

    if matched_sounds.is_empty() {
        return false;
    }

    let mut suppress = false;
    let app = rt.app.clone();
    for (sound_id, passthrough) in matched_sounds {
        if !passthrough {
            suppress = true;
        }
        if let Some(state) = app.try_state::<AppState>() {
            let info = {
                let lib = state.lib.lock().unwrap_or_else(|p| p.into_inner());
                lib.sound(&sound_id).map(|s| {
                    (
                        lib.sound_path(s),
                        s.effective_gain(),
                        s.replay_mode,
                        s.duration,
                    )
                })
            };
            if let Some((path, gain, replay_mode, dur)) = info {
                let _ = state.engine.play_ext(sound_id.clone(), &path, gain, replay_mode, dur, None, None);
                let _ = app.emit("hotkey-played", sound_id);
            }
        }
    }

    suppress
}

fn should_suppress(table: &Mutex<InputTable>, code: &str) -> bool {
    let t = table.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(p) = &t.panic {
        if p.code == code && !p.passthrough {
            return true;
        }
    }
    if let Some(list) = t.sound_bindings.get(code) {
        if list.iter().any(|(_, pass)| !*pass) {
            return true;
        }
    }
    false
}

// --------------------------------------------------------------------------
// Win32 Hook & Raw Input Mesaj Döngüsü
// --------------------------------------------------------------------------

unsafe extern "system" fn keyboard_ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
        if kb.dwExtraInfo != OZAN_SYNTHETIC_TAG {
            let msg = wparam as u32;
            let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            if is_down || is_up {
                let ext = (kb.flags & LLKHF_EXTENDED) != 0;
                let (key_code, label) = format_vk(kb.vkCode, kb.scanCode, ext);
                if handle_button_event(key_code, label, is_down) {
                    return 1; // Passthrough kapalı: oyuna/uygulamaya iletme!
                }
            }
        }
    }
    CallNextHookEx(null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn mouse_ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        let ms = &*(lparam as *const MSLLHOOKSTRUCT);
        if ms.dwExtraInfo != OZAN_SYNTHETIC_TAG {
            let msg = wparam as u32;
            match msg {
                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    let is_down = msg == WM_XBUTTONDOWN;
                    let xbtn = ((ms.mouseData >> 16) & 0xFFFF) as u16;
                    let (btn_code, label) = match xbtn {
                        XBUTTON1_HI => ("MOUSE:X1".to_string(), "Mouse 4 (XBUTTON1)".to_string()),
                        XBUTTON2_HI => ("MOUSE:X2".to_string(), "Mouse 5 (XBUTTON2)".to_string()),
                        other => (format!("MOUSE:X{other}"), format!("Mouse X{other}")),
                    };
                    if handle_button_event(btn_code, label, is_down) {
                        return 1;
                    }
                }
                WM_MBUTTONDOWN | WM_MBUTTONUP => {
                    let is_down = msg == WM_MBUTTONDOWN;
                    if handle_button_event("MOUSE:M3".into(), "Mouse 3 (Orta Tuş)".into(), is_down) {
                        return 1;
                    }
                }
                _ => {}
            }
        }
    }
    CallNextHookEx(null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn raw_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => {
            process_wm_input(lparam as HRAWINPUT);
            0
        }
        WM_INPUT_DEVICE_CHANGE => {
            register_all_raw_devices(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn input_thread_main() {
    let hinst = GetModuleHandleW(null());
    let class_name: Vec<u16> = "OzanRawInputSink\0".encode_utf16().collect();
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(raw_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: null_mut(),
        hCursor: null_mut(),
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: class_name.as_ptr(),
    };
    RegisterClassW(&wc);

    let hwnd = CreateWindowExW(
        0,
        class_name.as_ptr(),
        class_name.as_ptr(),
        0,
        0,
        0,
        0,
        0,
        null_mut(),
        null_mut(),
        hinst,
        null(),
    );

    if !hwnd.is_null() {
        register_all_raw_devices(hwnd);
    }

    let _kb_hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_ll_proc), hinst, 0);
    let _ms_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_proc), hinst, 0);

    let mut msg: MSG = zeroed();
    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// Sistemdeki tüm Fare, Klavye ve Çok Tuşlu MMO Fare / HID cihazlarını (`UsagePage` sınırı olmadan)
/// arka planda (`RIDEV_INPUTSINK`) dinlemek üzere kaydeder.
unsafe fn register_all_raw_devices(hwnd: HWND) {
    let mut pages: HashSet<u16> = HashSet::from([0x01, 0x05, 0x09, 0x0C]);
    let mut num_devices: u32 = 0;
    if GetRawInputDeviceList(null_mut(), &mut num_devices, size_of::<RAWINPUTDEVICELIST>() as u32) == 0
        && num_devices > 0
    {
        let mut list = vec![RAWINPUTDEVICELIST::default(); num_devices as usize];
        if GetRawInputDeviceList(
            list.as_mut_ptr(),
            &mut num_devices,
            size_of::<RAWINPUTDEVICELIST>() as u32,
        ) != u32::MAX
        {
            for dev in &list {
                let mut info: RID_DEVICE_INFO = zeroed();
                info.cbSize = size_of::<RID_DEVICE_INFO>() as u32;
                let mut sz = info.cbSize;
                if GetRawInputDeviceInfoW(
                    dev.hDevice,
                    RIDI_DEVICEINFO,
                    &mut info as *mut _ as *mut c_void,
                    &mut sz,
                ) != u32::MAX
                    && info.dwType == RIM_TYPEHID
                {
                    let page = info.Anonymous.hid.usUsagePage;
                    if page != 0 {
                        pages.insert(page);
                    }
                }
            }
        }
    }

    let mut rids: Vec<RAWINPUTDEVICE> = Vec::new();
    // Standart Fare ve Klavye
    rids.push(RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
        hwndTarget: hwnd,
    });
    rids.push(RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x06,
        dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
        hwndTarget: hwnd,
    });
    // Tüm HID Usage Page'leri (Gamepad, Consumer, Vendor MMO fare sayfaları dahil)
    for page in pages {
        rids.push(RAWINPUTDEVICE {
            usUsagePage: page,
            usUsage: 0,
            dwFlags: RIDEV_PAGEONLY | RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        });
    }
    let _ = RegisterRawInputDevices(
        rids.as_ptr(),
        rids.len() as u32,
        size_of::<RAWINPUTDEVICE>() as u32,
    );
}

unsafe fn process_wm_input(hraw: HRAWINPUT) {
    let mut size: u32 = 0;
    let header_sz = size_of::<RAWINPUTHEADER>() as u32;
    if GetRawInputData(hraw, RID_INPUT, null_mut(), &mut size, header_sz) != 0 || size == 0 {
        return;
    }
    let mut buf = vec![0u8; size as usize];
    if GetRawInputData(hraw, RID_INPUT, buf.as_mut_ptr() as *mut c_void, &mut size, header_sz) != size {
        return;
    }
    let raw = &*(buf.as_ptr() as *const RAWINPUT);
    match raw.header.dwType {
        RIM_TYPEMOUSE => process_raw_mouse(raw),
        RIM_TYPEKEYBOARD => process_raw_keyboard(raw),
        RIM_TYPEHID => process_raw_hid(raw, &buf),
        _ => {}
    }
}

unsafe fn process_raw_mouse(raw: &RAWINPUT) {
    let ms = &raw.data.mouse;
    if ms.ulExtraInformation as usize == OZAN_SYNTHETIC_TAG {
        return;
    }
    let flags = ms.Anonymous.Anonymous.usButtonFlags;
    if (flags & RI_MOUSE_BUTTON_3_DOWN as u16) != 0 {
        handle_button_event("MOUSE:M3".into(), "Mouse 3 (Orta Tuş)".into(), true);
    }
    if (flags & RI_MOUSE_BUTTON_3_UP as u16) != 0 {
        handle_button_event("MOUSE:M3".into(), "Mouse 3 (Orta Tuş)".into(), false);
    }
    if (flags & RI_MOUSE_BUTTON_4_DOWN as u16) != 0 {
        handle_button_event("MOUSE:X1".into(), "Mouse 4 (XBUTTON1)".into(), true);
    }
    if (flags & RI_MOUSE_BUTTON_4_UP as u16) != 0 {
        handle_button_event("MOUSE:X1".into(), "Mouse 4 (XBUTTON1)".into(), false);
    }
    if (flags & RI_MOUSE_BUTTON_5_DOWN as u16) != 0 {
        handle_button_event("MOUSE:X2".into(), "Mouse 5 (XBUTTON2)".into(), true);
    }
    if (flags & RI_MOUSE_BUTTON_5_UP as u16) != 0 {
        handle_button_event("MOUSE:X2".into(), "Mouse 5 (XBUTTON2)".into(), false);
    }

    // `ulRawButtons` üzerinden 6..32 arası ek fare butonlarını yakala
    let Some(rt) = RUNTIME.get() else { return };
    let raw_btns = ms.ulRawButtons;
    let prev = {
        let mut g = rt.raw_mouse_buttons.lock().unwrap_or_else(|p| p.into_inner());
        let p = *g;
        *g = raw_btns;
        p
    };
    let diff = raw_btns ^ prev;
    if diff != 0 {
        // İlk 5 bit standart fare butonlarıdır; 5..32 arası ek MMO butonları.
        for bit in 5..32u32 {
            let mask = 1u32 << bit;
            if (diff & mask) != 0 {
                let is_down = (raw_btns & mask) != 0;
                let btn_num = bit + 1;
                handle_button_event(
                    format!("MOUSE:B{btn_num}"),
                    format!("Mouse {btn_num} (RAW)"),
                    is_down,
                );
            }
        }
    }
}

unsafe fn process_raw_keyboard(raw: &RAWINPUT) {
    let kb = &raw.data.keyboard;
    if kb.ExtraInformation as usize == OZAN_SYNTHETIC_TAG {
        return;
    }
    // Bazı çok tuşlu MMO fareleri sadece Raw Input klavye paketi gönderir.
    let vkey = kb.VKey as u32;
    if vkey == 0 || vkey == 0xFF {
        return;
    }
    let is_up = (kb.Flags & (LLKHF_UP as u16)) != 0 || kb.Message == WM_KEYUP || kb.Message == WM_SYSKEYUP;
    let ext = (kb.Flags & 0x02) != 0; // RI_KEY_E0
    let (code, label) = format_vk(vkey, kb.MakeCode as u32, ext);
    handle_button_event(code, label, !is_up);
}

unsafe fn process_raw_hid(raw: &RAWINPUT, full_buf: &[u8]) {
    let Some(rt) = RUNTIME.get() else { return };
    let hdev: HANDLE = raw.header.hDevice;
    let dev_key = hdev as isize;

    // Cihazın VendorId / ProductId bilgisini al
    let mut dev_info: RID_DEVICE_INFO = zeroed();
    dev_info.cbSize = size_of::<RID_DEVICE_INFO>() as u32;
    let mut info_sz = dev_info.cbSize;
    let (vid, pid) = if GetRawInputDeviceInfoW(
        hdev,
        RIDI_DEVICEINFO,
        &mut dev_info as *mut _ as *mut c_void,
        &mut info_sz,
    ) != u32::MAX
        && dev_info.dwType == RIM_TYPEHID
    {
        (
            dev_info.Anonymous.hid.dwVendorId as u16,
            dev_info.Anonymous.hid.dwProductId as u16,
        )
    } else {
        (0u16, 0u16)
    };

    let hid = &raw.data.hid;
    let report_size = hid.dwSizeHid as usize;
    let report_count = hid.dwCount as usize;
    if report_size == 0 || report_count == 0 {
        return;
    }
    let reports_offset = size_of::<RAWINPUTHEADER>() + 8; // dwSizeHid (4) + dwCount (4)
    if full_buf.len() < reports_offset + report_size * report_count {
        return;
    }
    let reports_slice = &full_buf[reports_offset..reports_offset + report_size * report_count];

    // 1. Standart HID Preparsed Data + HidP_GetUsagesEx ile tüm HID buton kullanımlarını çözümle
    let mut preparsed_sz: u32 = 0;
    let mut handled_by_hidp = false;
    if GetRawInputDeviceInfoW(hdev, RIDI_PREPARSEDDATA, null_mut(), &mut preparsed_sz) == 0 && preparsed_sz > 0 {
        let mut preparsed = vec![0u8; preparsed_sz as usize];
        if GetRawInputDeviceInfoW(
            hdev,
            RIDI_PREPARSEDDATA,
            preparsed.as_mut_ptr() as *mut c_void,
            &mut preparsed_sz,
        ) != u32::MAX
        {
            let ppd = preparsed.as_ptr() as PHIDP_PREPARSED_DATA;
            for rep in reports_slice.chunks_exact(report_size) {
                let mut usages = [USAGE_AND_PAGE::default(); 64];
                let mut usage_len = usages.len() as u32;
                let status = HidP_GetUsagesEx(
                    HidP_Input,
                    0,
                    usages.as_mut_ptr(),
                    &mut usage_len,
                    ppd,
                    rep.as_ptr(),
                    report_size as u32,
                );
                if status >= 0 {
                    handled_by_hidp = true;
                    let mut current_set: HashSet<(u16, u16)> = HashSet::new();
                    for u in &usages[..usage_len as usize] {
                        // Standart sol/sağ fare tıklamasını (Page 0x09, Usage 1..=2) atla
                        if u.UsagePage == 0x09 && (u.Usage == 1 || u.Usage == 2) {
                            continue;
                        }
                        current_set.insert((u.UsagePage, u.Usage));
                    }
                    let prev_set = {
                        let mut map = rt.hid_pressed.lock().unwrap_or_else(|p| p.into_inner());
                        map.insert(dev_key, current_set.clone()).unwrap_or_default()
                    };
                    for &(up, us) in current_set.difference(&prev_set) {
                        let code = format!("HID:{vid:04x}:{pid:04x}:{up:04x}:{us:04x}");
                        let label = format!("HID Buton ({up:02X}:{us})");
                        handle_button_event(code, label, true);
                    }
                    for &(up, us) in prev_set.difference(&current_set) {
                        let code = format!("HID:{vid:04x}:{pid:04x}:{up:04x}:{us:04x}");
                        let label = format!("HID Buton ({up:02X}:{us})");
                        handle_button_event(code, label, false);
                    }
                }
            }
        }
    }

    // 2. Vendor-özel (ham bit maskesi kullanan) MMO fareleri için rapor bit-farkı yakalayıcı
    if !handled_by_hidp {
        for rep in reports_slice.chunks_exact(report_size) {
            let prev = {
                let mut map = rt.hid_raw_prev.lock().unwrap_or_else(|p| p.into_inner());
                let p = map.get(&dev_key).cloned();
                map.insert(dev_key, rep.to_vec());
                p
            };
            if let Some(prev) = prev {
                if prev.len() == rep.len() {
                    for (byte_idx, (&cur_b, &prev_b)) in rep.iter().zip(prev.iter()).enumerate() {
                        let diff = cur_b ^ prev_b;
                        if diff != 0 {
                            for bit in 0..8u8 {
                                let mask = 1u8 << bit;
                                if (diff & mask) != 0 {
                                    let is_down = (cur_b & mask) != 0;
                                    let btn_id = byte_idx * 8 + bit as usize + 1;
                                    let code = format!("HIDRAW:{vid:04x}:{pid:04x}:{byte_idx}:{bit}");
                                    let label = format!("MMO Fare Tuşu #{btn_id}");
                                    handle_button_event(code, label, is_down);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Sanal tuş kodunu (`vkCode`) benzersiz koda ve kullanıcı dostu etikete çevirir.
pub fn format_vk(vk: u32, scan: u32, ext: bool) -> (String, String) {
    let code = if ext {
        format!("VK:{vk}:E")
    } else {
        format!("VK:{vk}")
    };
    let label = match vk {
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => if ext { "Numpad Enter".into() } else { "Enter".into() },
        0x10 | 0xA0 => "Sol Shift".into(),
        0xA1 => "Sağ Shift".into(),
        0x11 | 0xA2 => "Sol Ctrl".into(),
        0xA3 => "Sağ Ctrl".into(),
        0x12 | 0xA4 => "Sol Alt".into(),
        0xA5 => "Sağ Alt".into(),
        0x13 => "Pause".into(),
        0x14 => "Caps Lock".into(),
        0x1B => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "Page Up".into(),
        0x22 => "Page Down".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Sol Ok".into(),
        0x26 => "Yukarı Ok".into(),
        0x27 => "Sağ Ok".into(),
        0x28 => "Aşağı Ok".into(),
        0x2C => "Print Screen".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x30..=0x39 => format!("{}", (vk as u8) as char),
        0x41..=0x5A => format!("{}", (vk as u8) as char),
        0x5B => "Sol Win".into(),
        0x5C => "Sağ Win".into(),
        0x60..=0x69 => format!("Numpad {}", vk - 0x60),
        0x6A => "Numpad *".into(),
        0x6B => "Numpad +".into(),
        0x6D => "Numpad -".into(),
        0x6E => "Numpad .".into(),
        0x6F => "Numpad /".into(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x90 => "Num Lock".into(),
        0x91 => "Scroll Lock".into(),
        0xBA => "Ş / ;".into(),
        0xBB => "= / +".into(),
        0xBC => "Ö / ,".into(),
        0xBD => "- / _".into(),
        0xBE => "Ç / .".into(),
        0xBF => "/".into(),
        0xC0 => "\" / `".into(),
        0xDB => "Ğ / [".into(),
        0xDC => "İ / \\".into(),
        0xDD => "Ü / ]".into(),
        0xDE => "İ / '".into(),
        _ => {
            if scan != 0 {
                format!("Tuş (VK {vk} / SC {scan})")
            } else {
                format!("Tuş (VK {vk})")
            }
        }
    };
    (code, label)
}
