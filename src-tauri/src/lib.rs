use std::process::Command;
use std::collections::{HashMap, VecDeque, HashSet};
use std::sync::Mutex;
use std::io::Read;
use std::fs::File;
use std::io::{Seek, SeekFrom, BufRead, BufReader};
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sysinfo::{System, Pid};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use base64::Engine;
use flate2::read::GzDecoder;
use tokio::net::UdpSocket;
use tokio::time::sleep;

use once_cell::sync::Lazy;
use tauri::Manager;
static VRCHAT_PROCESSES: Lazy<Mutex<HashMap<u32, u32>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static PENDING_PROFILES: Lazy<Mutex<VecDeque<u32>>> = Lazy::new(|| Mutex::new(VecDeque::new()));
static MISSED_DETECTIONS: Lazy<Mutex<HashMap<u32, u32>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static STOPPING_PROFILES: Lazy<Mutex<HashSet<u32>>> = Lazy::new(|| Mutex::new(HashSet::new()));
static PROFILE_SETTINGS: Lazy<Mutex<HashMap<u32, Value>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static PROFILE_LOG_FILES: Lazy<Mutex<HashMap<u32, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static PROFILE_LOG_MONITORS: Lazy<Mutex<HashMap<u32, LogMonitorState>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static PROFILE_OSC_PORTS: Lazy<Mutex<HashMap<u32, OscPorts>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static PROFILE_ROUND_OVER_MONITORS: Lazy<Mutex<HashMap<u32, LogMonitorState>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static TERROR_NAME_BY_ID: Lazy<HashMap<i32, String>> = Lazy::new(load_terror_name_map);
const EMBEDDED_TERRORS_JSON: &str = include_str!("../../src/assets/terrors.json");
const ROUND_OVER_KEYWORD: &str = "RoundOver";

#[derive(Clone, Copy)]
struct OscPorts {
    in_port: u16,
    out_port: u16,
}

#[derive(Serialize, Deserialize)]
struct OscPortsResult {
    in_port: u16,
    out_port: u16,
}

#[derive(Serialize, Deserialize)]
struct AttachExistingResult {
    attached: bool,
    profile: Option<u32>,
    process_id: Option<u32>,
    in_port: Option<u16>,
    out_port: Option<u16>,
    message: String,
}

#[derive(Clone, Default)]
struct LogMonitorState {
    path: String,
    position: u64,
}

#[derive(Serialize)]
struct LogSummaryEntry {
    timestamp: String,
    kind: String,
    message: String,
}

fn default_osc_ports(profile: u32) -> OscPorts {
    let base = 9000u32.saturating_add(profile.saturating_mul(10));
    let in_port = u16::try_from(base).unwrap_or(9000);
    let out_port = u16::try_from(base.saturating_add(1)).unwrap_or(9001);
    OscPorts { in_port, out_port }
}

fn get_or_init_profile_osc_ports(profile: u32) -> OscPorts {
    let mut map = PROFILE_OSC_PORTS.lock().unwrap();
    if let Some(&ports) = map.get(&profile) {
        return ports;
    }
    let ports = default_osc_ports(profile);
    map.insert(profile, ports);
    ports
}

fn set_profile_osc_ports(profile: u32, ports: OscPorts) {
    let mut map = PROFILE_OSC_PORTS.lock().unwrap();
    map.insert(profile, ports);
}

fn detect_next_profile(used: &HashSet<u32>) -> u32 {
    (1..).find(|n| !used.contains(n)).unwrap_or(1)
}

fn parse_local_port_from_endpoint(endpoint: &str) -> Option<u16> {
    endpoint
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
}

fn find_udp_9000_ports_for_pid(pid: u32) -> Vec<u16> {
    let output = match Command::new("netstat")
        .args(["-ano", "-p", "udp"])
        .output()
    {
        Ok(out) => out,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut ports = Vec::new();

    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        if !cols[0].eq_ignore_ascii_case("udp") {
            continue;
        }
        let owning_pid = match cols.last().and_then(|s| s.parse::<u32>().ok()) {
            Some(v) => v,
            None => continue,
        };
        if owning_pid != pid {
            continue;
        }
        let local = cols[1];
        let Some(port) = parse_local_port_from_endpoint(local) else {
            continue;
        };
        if (9000..=9999).contains(&port) {
            ports.push(port);
        }
    }

    ports.sort_unstable();
    ports.dedup();
    ports
}

fn detect_osc_ports_for_pid(pid: u32, profile_fallback: u32) -> OscPorts {
    let ports = find_udp_9000_ports_for_pid(pid);
    if ports.len() >= 2 {
        return OscPorts {
            in_port: ports[0],
            out_port: ports[1],
        };
    }
    if ports.len() == 1 {
        let in_port = ports[0];
        let out_port = if in_port < 9999 { in_port + 1 } else { in_port };
        return OscPorts { in_port, out_port };
    }
    default_osc_ports(profile_fallback)
}

fn clear_profile_settings(profile: u32) {
    let mut settings = PROFILE_SETTINGS.lock().unwrap();
    settings.remove(&profile);
    let mut log_files = PROFILE_LOG_FILES.lock().unwrap();
    log_files.remove(&profile);
    let mut osc_ports = PROFILE_OSC_PORTS.lock().unwrap();
    osc_ports.remove(&profile);
    let mut round_monitors = PROFILE_ROUND_OVER_MONITORS.lock().unwrap();
    round_monitors.remove(&profile);
}

struct StopGuard {
    profile: u32,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        let mut s = STOPPING_PROFILES.lock().unwrap();
        s.remove(&self.profile);
        eprintln!("[STOP GUARD] Profile {} stop flag removed", self.profile);
    }
}

#[derive(Serialize, Deserialize)]
struct VRChatResult {
    success: bool,
    message: String,
    process_id: Option<u32>,
    waiting_for_main_process: Option<bool>,
}

#[tauri::command]
async fn launch_vrchat(profile: u32) -> Result<VRChatResult, String> {
    let vrchat_path = r"C:\Program Files (x86)\Steam\steamapps\common\VRChat\start_protected_game.exe";
    let osc_ports = get_or_init_profile_osc_ports(profile);

    {
        let processes = VRCHAT_PROCESSES.lock().unwrap();
        if let Some(&existing_pid) = processes.get(&profile) {
            let mut system = System::new_all();
            system.refresh_all();
            if system.process(Pid::from(existing_pid as usize)).is_some() {
                return Ok(VRChatResult {
                    success: false,
                    message: format!("Profile {} is already running (PID: {})", profile, existing_pid),
                    process_id: Some(existing_pid),
                    waiting_for_main_process: Some(false),
                });
            }
        }
    }

    match Command::new(vrchat_path)
        .args(&[
            "--no-vr",
            &format!("--profile={}", profile),
            &format!("--osc={}:127.0.0.1:{}", osc_ports.in_port, osc_ports.out_port),
        ])
        .spawn()
    {
        Ok(child) => {
            let launcher_pid = child.id();
            eprintln!("[LAUNCH] Profile {} EAC launcher started (PID: {})", profile, launcher_pid);

            {
                let mut pending = PENDING_PROFILES.lock().unwrap();
                pending.push_back(profile);
            }

            Ok(VRChatResult {
                success: true,
                message: format!("VRChat Profile {} launched, waiting for main process", profile),
                process_id: Some(launcher_pid),
                waiting_for_main_process: Some(true),
            })
        }
        Err(e) => Ok(VRChatResult {
            success: false,
            message: format!("Failed to launch VRChat: {}", e),
            process_id: None,
            waiting_for_main_process: Some(false),
        }),
    }
}

#[tauri::command]
async fn send_osc_test(profile: u32) -> Result<String, String> {
    let in_port = get_or_init_profile_osc_ports(profile).in_port;
    let addr = format!("127.0.0.1:{}", in_port);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .subsec_nanos();
    let num = (nanos % 999) + 1;
    let text = format!("{}", num);

    let mut packet = Vec::new();
    let addr_pattern = b"/chatbox/input";
    packet.extend_from_slice(addr_pattern);
    packet.push(0);
    while packet.len() % 4 != 0 { packet.push(0); }
    packet.extend_from_slice(b",sT");
    packet.push(0);
    while packet.len() % 4 != 0 { packet.push(0); }
    let text_bytes = text.as_bytes();
    packet.extend_from_slice(text_bytes);
    packet.push(0);
    while packet.len() % 4 != 0 { packet.push(0); }

    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| format!("bind: {}", e))?;
    socket.send_to(&packet, &addr).await.map_err(|e| format!("send: {}", e))?;

    eprintln!("[OSC TEST] Sent '{}' to {}", text, addr);
    Ok(text)
}

fn append_osc_string_block(packet: &mut Vec<u8>, text: &str) {
    packet.extend_from_slice(text.as_bytes());
    packet.push(0);
    while packet.len() % 4 != 0 {
        packet.push(0);
    }
}

async fn send_osc_int(profile: u32, address: &str, value: i32) -> Result<(), String> {
    let in_port = get_or_init_profile_osc_ports(profile).in_port;
    let target = format!("127.0.0.1:{}", in_port);

    let mut packet = Vec::new();
    append_osc_string_block(&mut packet, address);
    append_osc_string_block(&mut packet, ",i");
    packet.extend_from_slice(&value.to_be_bytes());

    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| format!("bind: {}", e))?;
    socket.send_to(&packet, &target).await.map_err(|e| format!("send: {}", e))?;
    Ok(())
}

async fn send_osc_float(profile: u32, address: &str, value: f32) -> Result<(), String> {
    let in_port = get_or_init_profile_osc_ports(profile).in_port;
    let target = format!("127.0.0.1:{}", in_port);

    let mut packet = Vec::new();
    append_osc_string_block(&mut packet, address);
    append_osc_string_block(&mut packet, ",f");
    packet.extend_from_slice(&value.to_be_bytes());

    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| format!("bind: {}", e))?;
    socket.send_to(&packet, &target).await.map_err(|e| format!("send: {}", e))?;
    Ok(())
}

#[tauri::command]
async fn send_osc_jump(profile: u32) -> Result<(), String> {
    // Match the automator's /input/Jump int send behavior, with a short 1->0 pulse.
    send_osc_int(profile, "/input/Jump", 1).await?;
    sleep(Duration::from_millis(40)).await;
    send_osc_int(profile, "/input/Jump", 0).await?;
    Ok(())
}

#[derive(Clone, Copy)]
struct OscClickCursorState {
    prev_x: i32,
    prev_y: i32,
    tab_held: bool,
}

fn tracked_pid_for_profile(profile: u32) -> Option<u32> {
    let map = VRCHAT_PROCESSES.lock().unwrap();
    map.get(&profile).copied()
}

fn prepare_profile_window_for_osc_click(profile: u32) -> Result<Option<OscClickCursorState>, String> {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse as km;
    use windows_sys::Win32::UI::WindowsAndMessaging as wm;
    use std::mem::{size_of, zeroed};

    let Some(pid) = tracked_pid_for_profile(profile) else {
        return Ok(None);
    };

    unsafe {
        let Some(hwnd) = find_hwnd_by_pid(pid) else {
            return Ok(None);
        };

        // If minimized, restore to make sure center coordinates are valid.
        let mut wp: wm::WINDOWPLACEMENT = zeroed();
        wp.length = size_of::<wm::WINDOWPLACEMENT>() as u32;
        if wm::GetWindowPlacement(hwnd, &mut wp) != 0 && wp.showCmd == wm::SW_SHOWMINIMIZED as u32 {
            wm::ShowWindow(hwnd, wm::SW_RESTORE);
            thread::sleep(Duration::from_millis(60));
        }

        let mut prev = POINT { x: 0, y: 0 };
        if wm::GetCursorPos(&mut prev) == 0 {
            return Ok(None);
        }

        let mut rect: RECT = zeroed();
        if wm::GetWindowRect(hwnd, &mut rect) == 0 {
            return Ok(None);
        }

        let center_x = (rect.left + rect.right) / 2;
        let center_y = (rect.top + rect.bottom) / 2;
        wm::SetCursorPos(center_x, center_y);
        thread::sleep(Duration::from_millis(25));

        // Hold TAB briefly while OSC click is sent (desktop mode cursor assist behavior).
        let mut tab_down: km::INPUT = zeroed();
        tab_down.r#type = km::INPUT_KEYBOARD;
        tab_down.Anonymous.ki.wVk = km::VK_TAB;
        km::SendInput(1, &tab_down, size_of::<km::INPUT>() as i32);
        thread::sleep(Duration::from_millis(25));

        Ok(Some(OscClickCursorState {
            prev_x: prev.x,
            prev_y: prev.y,
            tab_held: true,
        }))
    }
}

fn finish_profile_window_osc_click(state: Option<OscClickCursorState>) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse as km;
    use windows_sys::Win32::UI::WindowsAndMessaging as wm;
    use std::mem::{size_of, zeroed};

    let Some(state) = state else {
        return;
    };

    unsafe {
        if state.tab_held {
            let mut tab_up: km::INPUT = zeroed();
            tab_up.r#type = km::INPUT_KEYBOARD;
            tab_up.Anonymous.ki.wVk = km::VK_TAB;
            tab_up.Anonymous.ki.dwFlags = km::KEYEVENTF_KEYUP;
            km::SendInput(1, &tab_up, size_of::<km::INPUT>() as i32);
        }
        wm::SetCursorPos(state.prev_x, state.prev_y);
    }
}

async fn send_osc_left_click(profile: u32) -> Result<(), String> {
    let prep_state = match prepare_profile_window_for_osc_click(profile) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("[OSC CLICK] prepare failed for profile {}: {}", profile, e);
            None
        }
    };

    // Desktop pickup/use is usually right-hand use in OSC.
    let result = async {
        send_osc_int(profile, "/input/UseRight", 1).await?;
        sleep(Duration::from_millis(50)).await;
        send_osc_int(profile, "/input/UseRight", 0).await?;
        // Some interactions are grab-based; pulse grab as fallback.
        send_osc_int(profile, "/input/GrabRight", 1).await?;
        sleep(Duration::from_millis(50)).await;
        send_osc_int(profile, "/input/GrabRight", 0).await?;
        Ok(())
    }
    .await;

    finish_profile_window_osc_click(prep_state);
    result
}

async fn send_osc_right_click(profile: u32) -> Result<(), String> {
    // Right click action mapped to item release.
    send_osc_int(profile, "/input/DropRight", 1).await?;
    sleep(Duration::from_millis(50)).await;
    send_osc_int(profile, "/input/DropRight", 0).await?;
    Ok(())
}

#[tauri::command]
async fn run_osc_auto_start(profile: u32) -> Result<(), String> {
    // Right click once.
    send_osc_right_click(profile).await?;
    sleep(Duration::from_millis(500)).await;

    // Move forward for 2 seconds.
    send_osc_float(profile, "/input/Vertical", 1.0).await?;
    sleep(Duration::from_millis(2000)).await;
    send_osc_float(profile, "/input/Vertical", 0.0).await?;
    sleep(Duration::from_millis(80)).await;

    // Move left for 0.5 seconds.
    send_osc_float(profile, "/input/Horizontal", -1.0).await?;
    sleep(Duration::from_millis(120)).await;
    send_osc_float(profile, "/input/Horizontal", 0.0).await?;
    sleep(Duration::from_millis(80)).await;

    // left click 5 times.
    for _ in 0..5 {
        send_osc_left_click(profile).await?;
        sleep(Duration::from_millis(500)).await;
    }

    Ok(())
}

#[tauri::command]
async fn stop_vrchat(profile: u32) -> Result<VRChatResult, String> {
    {
        let mut pending = PENDING_PROFILES.lock().unwrap();
        pending.retain(|p| *p != profile);
    }

    {
        let mut s = STOPPING_PROFILES.lock().unwrap();
        s.insert(profile);
    }
    let _stop_guard = StopGuard { profile };

    let mut system = System::new_all();
    system.refresh_all();

    let stored_pid = {
        let processes = VRCHAT_PROCESSES.lock().unwrap();
        processes.get(&profile).copied()
    };

    fn kill_and_wait(pid: u32) -> bool {
        let mut s = System::new_all();
        s.refresh_all();
        if let Some(process) = s.process(Pid::from(pid as usize)) {
            let _ = process.kill();
        }
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(200));
            s.refresh_all();
            if s.process(Pid::from(pid as usize)).is_none() {
                return true;
            }
        }
        false
    }

    if let Some(pid) = stored_pid {
        system.refresh_all();
        if let Some(process) = system.process(Pid::from(pid as usize)) {
            let name = process.name().to_lowercase();
            let exe = process.exe().map(|p| p.to_string_lossy().to_string().to_lowercase()).unwrap_or_default();
            if name.contains("vrchat") || exe.contains("vrchat") {
                if kill_and_wait(pid) {
                    let mut processes = VRCHAT_PROCESSES.lock().unwrap();
                    processes.remove(&profile);
                    clear_profile_settings(profile);
                    return Ok(VRChatResult {
                        success: true,
                        message: format!("VRChat Profile {} stopped", profile),
                        process_id: Some(pid),
                        waiting_for_main_process: Some(false),
                    });
                } else {
                    return Ok(VRChatResult {
                        success: false,
                        message: format!("Failed to kill Profile {} process (PID: {})", profile, pid),
                        process_id: Some(pid),
                        waiting_for_main_process: Some(false),
                    });
                }
            }
        }
    }

    let mut found_pid: Option<u32> = None;
    for (pid, process) in system.processes() {
        let cmd_line = process.cmd().join(" ");
        if cmd_line.contains(&format!("--profile={}", profile)) {
            found_pid = Some(pid.as_u32());
            break;
        }
    }

    if let Some(pid) = found_pid {
        if kill_and_wait(pid) {
            let mut processes = VRCHAT_PROCESSES.lock().unwrap();
            processes.retain(|&p, &mut _| p != profile);
            processes.remove(&profile);
            clear_profile_settings(profile);
            return Ok(VRChatResult {
                success: true,
                message: format!("VRChat Profile {} stopped (PID: {})", profile, pid),
                process_id: Some(pid),
                waiting_for_main_process: Some(false),
            });
        } else {
            return Ok(VRChatResult {
                success: false,
                message: format!("Failed to kill Profile {} process (PID: {})", profile, pid),
                process_id: Some(pid),
                waiting_for_main_process: Some(false),
            });
        }
    }

    clear_profile_settings(profile);
    Ok(VRChatResult {
        success: false,
        message: format!("No process found for Profile {}", profile),
        process_id: None,
        waiting_for_main_process: Some(false),
    })
}

#[tauri::command]
async fn get_running_vrchat() -> Result<HashMap<u32, u32>, String> {
    let processes = VRCHAT_PROCESSES.lock().unwrap();
    Ok(processes.clone())
}

#[tauri::command]
fn get_profile_osc_ports(profile: u32) -> Result<OscPortsResult, String> {
    let ports = get_or_init_profile_osc_ports(profile);
    Ok(OscPortsResult {
        in_port: ports.in_port,
        out_port: ports.out_port,
    })
}

#[tauri::command]
async fn attach_existing_vrchat() -> Result<AttachExistingResult, String> {
    let mut system = System::new_all();
    system.refresh_all();

    let tracked_pids: HashSet<u32> = {
        let tracked = VRCHAT_PROCESSES.lock().unwrap();
        tracked.values().copied().collect()
    };

    let mut candidate_pid: Option<u32> = None;
    let mut candidate_start: Option<u64> = None;

    for (pid, process) in system.processes() {
        let name = process.name().to_string().to_lowercase();
        if !name.contains("vrchat") || name.contains("start_protected_game") || name.contains("unity") {
            continue;
        }
        let pid_u32 = pid.as_u32();
        if tracked_pids.contains(&pid_u32) {
            continue;
        }
        let start_time = process.start_time();
        if candidate_start.is_none() || start_time < candidate_start.unwrap_or(u64::MAX) {
            candidate_start = Some(start_time);
            candidate_pid = Some(pid_u32);
        }
    }

    let Some(found_pid) = candidate_pid else {
        return Ok(AttachExistingResult {
            attached: false,
            profile: None,
            process_id: None,
            in_port: None,
            out_port: None,
            message: "No untracked running VRChat process found".to_string(),
        });
    };

    let mut tracked = VRCHAT_PROCESSES.lock().unwrap();
    let used: HashSet<u32> = tracked.keys().copied().collect();
    let profile = detect_next_profile(&used);
    tracked.insert(profile, found_pid);
    drop(tracked);

    let ports = detect_osc_ports_for_pid(found_pid, profile);
    set_profile_osc_ports(profile, ports);

    Ok(AttachExistingResult {
        attached: true,
        profile: Some(profile),
        process_id: Some(found_pid),
        in_port: Some(ports.in_port),
        out_port: Some(ports.out_port),
        message: format!("Attached running VRChat PID {} as Profile {}", found_pid, profile),
    })
}

#[tauri::command]
async fn debug_vrchat_processes() -> Result<Vec<String>, String> {
    let mut system = System::new_all();
    system.refresh_all();
    let mut debug_info = Vec::new();

    for (pid, process) in system.processes() {
        let name = process.name().to_string();
        let name_lower = name.to_lowercase();
        let exe_path = process.exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();

        if name_lower.contains("vrchat") ||
           exe_path.contains("vrchat") ||
           name_lower.contains("start_protected_game") ||
           name_lower.contains("easyanticheat") ||
           name_lower.contains("eac") {
            let cmd_line = process.cmd().join(" ");
            let parent_pid = process.parent().map(|p| p.as_u32()).unwrap_or(0);
            let start_time = process.start_time();

            let profile = extract_profile_from_cmd(&cmd_line);
            let profile_str = profile.map(|p| format!("Profile={}", p)).unwrap_or_else(|| "Profile=none".to_string());

            debug_info.push(format!(
                "PID: {}, Name: {}, {}, Parent: {}, StartTime: {}, Exe: {}, Args: {}",
                pid, name, profile_str, parent_pid, start_time, exe_path, cmd_line
            ));
        }
    }

    debug_info.sort();
    Ok(debug_info)
}

fn extract_profile_from_cmd(cmd_line: &str) -> Option<u32> {
    if let Some(start) = cmd_line.find("--profile=") {
        let profile_str = &cmd_line[start + 10..];
        if let Some(end) = profile_str.find(' ').or(Some(profile_str.len())) {
            let profile_num_str = &profile_str[..end];
            return profile_num_str.parse().ok();
        }
    }
    None
}

fn spawn_vrchat_pid_monitor() {
    const INTERVAL_SECONDS: u64 = 3;

    thread::spawn(|| {
        loop {
            let mut system = System::new_all();
            system.refresh_all();

            let mut detected_processes = Vec::new();
            for (pid, process) in system.processes() {
                let name = process.name().to_string().to_lowercase();

                let is_vrchat = name.contains("vrchat")
                    && !name.contains("start_protected_game")
                    && !name.contains("unity");
                if is_vrchat {
                    let cmd_line = process.cmd().join(" ");
                        let profile = extract_profile_from_cmd(&cmd_line);
                    detected_processes.push((pid.as_u32(), profile));
                }
            }

            {
                let mut stored = VRCHAT_PROCESSES.lock().unwrap();

                let detected_pids: Vec<u32> = detected_processes.iter().map(|(pid, _)| *pid).collect();

                {
                    let mut missed = MISSED_DETECTIONS.lock().unwrap();
                    let mut to_remove = Vec::new();

                    let stopping_snapshot = STOPPING_PROFILES.lock().unwrap().clone();

                    for (&profile, &pid) in stored.iter() {
                        if stopping_snapshot.contains(&profile) {
                            continue;
                        }

                        if !detected_pids.contains(&pid) {
                            let cnt = missed.entry(profile).or_insert(0);
                            *cnt += 1;
                            if *cnt >= 2 {
                                to_remove.push((profile, pid));
                            }
                        } else {
                            missed.remove(&profile);
                        }
                    }

                    for (profile, _old_pid) in to_remove {
                        if let Some(removed_pid) = stored.remove(&profile) {
                            eprintln!("[PID MONITOR] Profile {} PID {} removed (missed 2x)", profile, removed_pid);
                        }
                        clear_profile_settings(profile);
                        missed.remove(&profile);
                    }
                }

                let stopping_snapshot = STOPPING_PROFILES.lock().unwrap().clone();

                for (new_pid, cmd_profile) in detected_processes {
                    if let Some(profile) = cmd_profile {
                        if stopping_snapshot.contains(&profile) {
                            eprintln!("[PID MONITOR] Profile {} is stopping, ignoring PID {}", profile, new_pid);
                            continue;
                        }

                        if let Some(&old_pid) = stored.get(&profile) {
                            if old_pid != new_pid {
                                eprintln!("[PID MONITOR] Profile {} PID changed: {} -> {}", profile, old_pid, new_pid);
                                stored.insert(profile, new_pid);
                                let ports = detect_osc_ports_for_pid(new_pid, profile);
                                set_profile_osc_ports(profile, ports);
                            }
                        } else {
                            eprintln!("[PID MONITOR] Profile {} registered: PID {}", profile, new_pid);
                            stored.insert(profile, new_pid);
                            let ports = detect_osc_ports_for_pid(new_pid, profile);
                            set_profile_osc_ports(profile, ports);
                        }
                    } else {
                        let already_stored = stored.values().any(|&pid| pid == new_pid);
                        if !already_stored {
                            let mut pending = PENDING_PROFILES.lock().unwrap();
                            let mut assigned = false;
                            while let Some(profile) = pending.pop_front() {
                                if stopping_snapshot.contains(&profile) {
                                    continue;
                                }
                                eprintln!("[PID MONITOR] Assigned pending Profile {} to PID {}", profile, new_pid);
                                stored.insert(profile, new_pid);
                                let ports = detect_osc_ports_for_pid(new_pid, profile);
                                set_profile_osc_ports(profile, ports);
                                assigned = true;
                                break;
                            }
                            if !assigned {
                                let used: HashSet<u32> = stored.keys().copied().collect();
                                let next_profile = (1..).find(|n| !used.contains(n)).unwrap_or(1);
                                if !stopping_snapshot.contains(&next_profile) {
                                    eprintln!("[PID MONITOR] Auto-assigned Profile {} to PID {} (no --profile)", next_profile, new_pid);
                                    stored.insert(next_profile, new_pid);
                                    let ports = detect_osc_ports_for_pid(new_pid, next_profile);
                                    set_profile_osc_ports(next_profile, ports);
                                }
                            }
                        }
                    }
                }
            }

            thread::sleep(Duration::from_secs(INTERVAL_SECONDS));
        }
    });
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

fn get_default_vrchat_log_path_impl() -> Option<String> {
    let home = std::env::var("USERPROFILE").ok()?;
    Some(format!("{}\\AppData\\LocalLow\\VRChat\\VRChat", home))
}

#[tauri::command]
fn get_default_vrchat_log_path() -> Result<String, String> {
    get_default_vrchat_log_path_impl().ok_or_else(|| "USERPROFILE not set".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    spawn_vrchat_pid_monitor();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            launch_vrchat,
            send_osc_test,
            send_osc_jump,
            run_osc_auto_start,
            stop_vrchat,
            get_running_vrchat,
            get_profile_osc_ports,
            attach_existing_vrchat,
            debug_vrchat_processes,
            is_eac_launcher_running,
            create_sub_window,
            decode_tracker_import,
            get_profile_settings,
            set_profile_settings,
            remove_profile_settings,
            click_inactive_window,
            get_default_vrchat_log_path,
            pick_vrchat_log_file,
            set_profile_log_file,
            get_profile_log_file,
            poll_profile_log_summary,
            poll_round_over
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
fn is_eac_launcher_running() -> Result<bool, String> {
    let mut system = System::new_all();
    system.refresh_all();

    for (_pid, process) in system.processes() {
        let name = process.name().to_string().to_lowercase();
        let exe_path = process.exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default().to_lowercase();
        if name.contains("start_protected_game") || exe_path.contains("start_protected_game") {
            return Ok(true);
        }
    }
    Ok(false)
}

#[tauri::command]
async fn create_sub_window(app: tauri::AppHandle, profile: u32) -> Result<(), String> {
    let label = format!("Mining_Setting_{}", profile);
    if let Some(window) = app.get_webview_window(&label) {
        window
            .set_focus()
            .map_err(|e| format!("Failed to focus existing sub-window: {}", e))?;
        return Ok(());
    }

    let window_url = format!("mining_setting.html?profile={}", profile);
    match tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(window_url.into()))
        .title(format!("Mining Setting - Profile {}", profile))
        .inner_size(600.0, 400.0)
        .build()
    {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Failed to create sub-window: {}", e)),
    }
}

#[tauri::command]
fn get_profile_settings(profile: u32) -> Result<Value, String> {
    let settings = PROFILE_SETTINGS.lock().unwrap();
    Ok(settings.get(&profile).cloned().unwrap_or_else(|| Value::Object(serde_json::Map::new())))
}

#[tauri::command]
fn set_profile_settings(profile: u32, settings: Value) -> Result<(), String> {
    if !settings.is_object() {
        return Err("settings must be a JSON object".to_string());
    }
    let mut state = PROFILE_SETTINGS.lock().unwrap();
    state.insert(profile, settings);
    Ok(())
}

#[tauri::command]
fn remove_profile_settings(profile: u32) -> Result<(), String> {
    clear_profile_settings(profile);
    Ok(())
}

#[tauri::command]
fn set_profile_log_file(profile: u32, path: String) -> Result<(), String> {
    let mut log_files = PROFILE_LOG_FILES.lock().unwrap();
    log_files.insert(profile, path);
    let mut monitors = PROFILE_LOG_MONITORS.lock().unwrap();
    monitors.remove(&profile);
    Ok(())
}

#[tauri::command]
fn get_profile_log_file(profile: u32) -> Result<String, String> {
    let log_files = PROFILE_LOG_FILES.lock().unwrap();
    log_files.get(&profile).cloned().ok_or_else(|| "Not set".to_string())
}

fn strip_utf8_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

fn load_terror_name_map() -> HashMap<i32, String> {
    fn parse_name_map(content: &str) -> HashMap<i32, String> {
        let content = strip_utf8_bom(content);
        let mut out = HashMap::new();
        let Ok(json) = serde_json::from_str::<Value>(content) else {
            return out;
        };
        let Some(arr) = json.as_array() else {
            return out;
        };
        for item in arr {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(id_val) = obj.get("id") else {
                continue;
            };
            let Some(name_val) = obj.get("Name") else {
                continue;
            };
            let Some(id) = id_val.as_i64() else {
                continue;
            };
            let Some(name) = name_val.as_str() else {
                continue;
            };
            out.insert(id as i32, name.to_string());
        }
        out
    }

    let embedded = parse_name_map(EMBEDDED_TERRORS_JSON);
    if !embedded.is_empty() {
        return embedded;
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let candidates = [
        cwd.join("../src/assets/terrors.json"),
        cwd.join("src/assets/terrors.json"),
        cwd.join("assets/terrors.json"),
    ];

    for path in candidates {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed = parse_name_map(&content);
        if !parsed.is_empty() {
            return parsed;
        }
    }
    HashMap::new()
}

fn get_timestamp_from_line(line: &str) -> String {
    line.chars().take(19).collect()
}

fn char_start_byte_index(s: &str, char_offset: usize) -> Option<usize> {
    if char_offset == 0 {
        return Some(0);
    }
    let mut count = 0usize;
    for (idx, _) in s.char_indices() {
        if count == char_offset {
            return Some(idx);
        }
        count += 1;
    }
    if count == char_offset {
        Some(s.len())
    } else {
        None
    }
}

fn extract_content_from_log_line(line: &str) -> &str {
    let Some(start) = char_start_byte_index(line, 34) else {
        return line;
    };
    &line[start..]
}

fn terror_label_from_id(id: i32) -> String {
    if id < 0 || id == 255 {
        return format!("ID {}", id);
    }
    if let Some(name) = TERROR_NAME_BY_ID.get(&id) {
        return format!("{} (ID {})", name, id);
    }
    format!("ID {}", id)
}

fn extract_i32_numbers_from_text(text: &str) -> Vec<i32> {
    let mut out = Vec::new();
    let mut buf = String::new();

    for ch in text.chars() {
        if ch.is_ascii_digit() || (ch == '-' && buf.is_empty()) {
            buf.push(ch);
            continue;
        }
        if !buf.is_empty() && buf != "-" {
            if let Ok(v) = buf.parse::<i32>() {
                out.push(v);
            }
        }
        buf.clear();
    }

    if !buf.is_empty() && buf != "-" {
        if let Ok(v) = buf.parse::<i32>() {
            out.push(v);
        }
    }
    out
}

fn parse_map_round_line(content: &str) -> Option<String> {
    const ROUND_MAP_LOCATION: &str = "This round is taking place at ";
    const ROUND_MAP_RTYPE: &str = " and the round type is ";
    if !content.starts_with(ROUND_MAP_LOCATION) {
        return None;
    }

    let map_start = ROUND_MAP_LOCATION.len();
    let left_paren = content.rfind('(')?;
    let right_paren = content[left_paren..].find(')')? + left_paren;
    if left_paren <= map_start || right_paren <= left_paren {
        return None;
    }

    let map_name = content[map_start..left_paren].trim();
    let map_id = content[left_paren + 1..right_paren].trim();
    let rt_idx = content.find(ROUND_MAP_RTYPE)?;
    let round_type = content[rt_idx + ROUND_MAP_RTYPE.len()..].trim();
    Some(format!(
        "Round Start | {} ({}) | RoundType {}",
        map_name, map_id, round_type
    ))
}

fn parse_killer_matrix_line(content: &str) -> Option<(String, String)> {
    const KILLER_MATRIX_KEYWORD: &str = "Killers have been set - ";
    const KILLER_MATRIX_UNKNOWN: &str = "Killers is unknown - ";
    const KILLER_MATRIX_REVEAL: &str = "Killers have been revealed - ";
    const KILLER_ROUND_TYPE_KEYWORD: &str = " // Round type is ";

    let (kind, start_idx) = if content.starts_with(KILLER_MATRIX_UNKNOWN) {
        ("killers-unknown", KILLER_MATRIX_UNKNOWN.len())
    } else if content.starts_with(KILLER_MATRIX_REVEAL) {
        ("killers-revealed", KILLER_MATRIX_REVEAL.len())
    } else if content.starts_with(KILLER_MATRIX_KEYWORD) {
        ("killers-set", KILLER_MATRIX_KEYWORD.len())
    } else {
        return None;
    };

    let rt_idx = content.find(KILLER_ROUND_TYPE_KEYWORD)?;
    if rt_idx <= start_idx {
        return None;
    }
    let round_type = content[rt_idx + KILLER_ROUND_TYPE_KEYWORD.len()..].trim();
    let ids_raw = content[start_idx..rt_idx].trim();
    let ids = extract_i32_numbers_from_text(ids_raw);

    // Filter out trailing zeros — non-multi-killer rounds always pad with ID 0.
    let last_nonzero = ids.iter().rposition(|&id| id != 0).map(|i| i + 1).unwrap_or(0);
    let ids = &ids[..last_nonzero];

    let mapped = if ids.is_empty() {
        "none".to_string()
    } else {
        ids.iter()
            .map(|&id| terror_label_from_id(id))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let msg = format!("Killers | {} | RoundType {} | {}", kind, round_type, mapped);
    Some((kind.to_string(), msg))
}

fn parse_log_summary_event(line: &str) -> Option<LogSummaryEntry> {
    const ROUND_OPTIN_KEYWORD: &str = "opted in";
    const ROUND_OPTOUT_KEYWORD: &str = "Player respawned";
    const ROUND_WON_KEYWORD: &str = "Player Won";
    const ROUND_LOST_KEYWORD: &str = "Player lost,";

    const ROUND_DEATH_KEYWORD: &str = "You died.";
    const ROUND_REBORN_KEYWORD: &str = "LOL JK, REBORN!";
    const ROUND_PAGE_FOUND: &str = "Page Collected - ";
    const ROUND_MAP_SWAPPED: &str = "Solstice has swapped the map to ";
    const ROUND_IS_SABO: &str = "You are the sussy baka of cringe naenae legend";
    const ROUND_DEATH_MSG_KEYWORD: &str = "[DEATH][";

    let timestamp = get_timestamp_from_line(line);
    let content = extract_content_from_log_line(line).trim();
    if content.is_empty() {
        return None;
    }

    if let Some(message) = parse_map_round_line(content) {
        return Some(LogSummaryEntry { timestamp, kind: "round-start".to_string(), message });
    }
    if let Some((kind, message)) = parse_killer_matrix_line(content) {
        return Some(LogSummaryEntry { timestamp, kind, message });
    }

    if content.starts_with(ROUND_OPTIN_KEYWORD) {
        return Some(LogSummaryEntry {
            timestamp,
            kind: "opt-in".to_string(),
            message: "Participation | Opted In".to_string(),
        });
    }
    if content.starts_with(ROUND_OPTOUT_KEYWORD) {
        return Some(LogSummaryEntry {
            timestamp,
            kind: "opt-out".to_string(),
            message: "Participation | Respawned / Opted Out".to_string(),
        });
    }
    if content.starts_with(ROUND_DEATH_KEYWORD) {
        return Some(LogSummaryEntry {
            timestamp,
            kind: "death".to_string(),
            message: "Round | You died".to_string(),
        });
    }
    if content.starts_with(ROUND_REBORN_KEYWORD) {
        return Some(LogSummaryEntry {
            timestamp,
            kind: "reborn".to_string(),
            message: "Round | Reborn".to_string(),
        });
    }
    if content.starts_with(ROUND_WON_KEYWORD) {
        return Some(LogSummaryEntry {
            timestamp,
            kind: "round-win".to_string(),
            message: "Round Result | Win".to_string(),
        });
    }
    if content.starts_with(ROUND_LOST_KEYWORD) {
        return Some(LogSummaryEntry {
            timestamp,
            kind: "round-lost".to_string(),
            message: "Round Result | Lost".to_string(),
        });
    }
    if content.starts_with(ROUND_OVER_KEYWORD) {
        return Some(LogSummaryEntry {
            timestamp,
            kind: "round-over".to_string(),
            message: "Round | RoundOver".to_string(),
        });
    }
    if content.starts_with(ROUND_PAGE_FOUND) {
        let v = content[ROUND_PAGE_FOUND.len()..].trim();
        return Some(LogSummaryEntry {
            timestamp,
            kind: "page".to_string(),
            message: format!("8 Pages | Collected {}", v),
        });
    }
    if content.starts_with(ROUND_MAP_SWAPPED) {
        let v = content[ROUND_MAP_SWAPPED.len()..].trim();
        return Some(LogSummaryEntry {
            timestamp,
            kind: "map-swap".to_string(),
            message: format!("Round | Map swapped to ID {}", v),
        });
    }
    if content.starts_with(ROUND_IS_SABO) {
        return Some(LogSummaryEntry {
            timestamp,
            kind: "saboteur".to_string(),
            message: "Role | Saboteur".to_string(),
        });
    }
    if content.starts_with(ROUND_DEATH_MSG_KEYWORD) {
        return Some(LogSummaryEntry {
            timestamp,
            kind: "death-msg".to_string(),
            message: format!("Death Log | {}", content),
        });
    }
    None
}

#[tauri::command]
fn poll_profile_log_summary(profile: u32) -> Result<Vec<LogSummaryEntry>, String> {
    let path = {
        let files = PROFILE_LOG_FILES.lock().unwrap();
        files.get(&profile).cloned().ok_or_else(|| "Log file is not set".to_string())?
    };

    let mut states = PROFILE_LOG_MONITORS.lock().unwrap();
    let state = states.entry(profile).or_default();
    if state.path != path {
        state.path = path.clone();
        state.position = 0;
    }

    let mut file = File::open(&path).map_err(|e| format!("Failed to open log file: {}", e))?;
    let metadata = file.metadata().map_err(|e| format!("Failed to read log metadata: {}", e))?;
    let file_len = metadata.len();

    if state.position > file_len {
        state.position = file_len;
    }
    if state.position == 0 {
        state.position = file_len;
    }

    file.seek(SeekFrom::Start(state.position))
        .map_err(|e| format!("Failed to seek log file: {}", e))?;

    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    let mut out = Vec::new();
    let mut read_bytes: u64 = 0;

    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).map_err(|e| format!("Failed to read log file: {}", e))?;
        if n == 0 {
            break;
        }
        read_bytes = read_bytes.saturating_add(n as u64);
        let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parse_log_summary_event(buf.trim_end_matches(['\r', '\n']))
        }));
        if let Ok(Some(entry)) = parsed {
            out.push(entry);
        }
    }

    state.position = state.position.saturating_add(read_bytes);
    if out.len() > 100 {
        let keep_from = out.len() - 100;
        Ok(out.split_off(keep_from))
    } else {
        Ok(out)
    }
}

#[tauri::command]
fn poll_round_over(profile: u32) -> Result<bool, String> {
    let path = {
        let files = PROFILE_LOG_FILES.lock().unwrap();
        files.get(&profile).cloned().ok_or_else(|| "Log file is not set".to_string())?
    };

    let mut states = PROFILE_ROUND_OVER_MONITORS.lock().unwrap();
    let state = states.entry(profile).or_insert_with(|| LogMonitorState {
        path: String::new(),
        position: 0,
    });
    if state.path != path {
        state.path = path.clone();
        state.position = 0;
    }

    let mut file = File::open(&path).map_err(|e| format!("Failed to open log file: {}", e))?;
    let metadata = file.metadata().map_err(|e| format!("Failed to read log metadata: {}", e))?;
    let file_len = metadata.len();

    if state.position > file_len {
        state.position = file_len;
    }
    if state.position == 0 {
        state.position = file_len;
    }

    file.seek(SeekFrom::Start(state.position))
        .map_err(|e| format!("Failed to seek log file: {}", e))?;

    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    let mut found = false;
    let mut read_bytes: u64 = 0;

    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).map_err(|e| format!("Failed to read log file: {}", e))?;
        if n == 0 {
            break;
        }
        read_bytes = read_bytes.saturating_add(n as u64);
        let content = extract_content_from_log_line(buf.trim_end_matches(['\r', '\n'])).trim();
        if content.starts_with(ROUND_OVER_KEYWORD) {
            found = true;
        }
    }

    state.position = state.position.saturating_add(read_bytes);
    Ok(found)
}

#[tauri::command]
fn decode_tracker_import(encoded: String) -> Result<Value, String> {
    let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    let raw_bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|e| format!("Base64 decode failed: {e}"))?;
    let raw_text = String::from_utf8(raw_bytes)
        .map_err(|e| format!("Decoded text is not UTF-8: {e}"))?;

    let numbers: Vec<u8> = raw_text
        .split(',')
        .filter_map(|s| s.trim().parse::<u16>().ok())
        .filter(|n| *n <= 255)
        .map(|n| n as u8)
        .collect();
    if numbers.is_empty() {
        return Err("No byte payload found in import data".to_string());
    }

    let mut decoder = GzDecoder::new(&numbers[..]);
    let mut json_text = String::new();
    decoder
        .read_to_string(&mut json_text)
        .map_err(|e| format!("Gzip decode failed: {e}"))?;

    serde_json::from_str::<Value>(&json_text)
        .map_err(|e| format!("JSON parse failed: {e}"))
}

use std::sync::atomic::AtomicPtr;
type HWND = *mut std::ffi::c_void;
static ENUM_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

unsafe fn find_hwnd_by_pid(pid: u32) -> Option<HWND> {
    use windows_sys::Win32::UI::WindowsAndMessaging as wm;
    ENUM_HWND.store(std::ptr::null_mut(), std::sync::atomic::Ordering::SeqCst);
    extern "system" fn enum_callback(hwnd: HWND, lparam: isize) -> i32 {
        let target_pid = lparam as u32;
        let mut actual_pid: u32 = 0;
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                hwnd, &mut actual_pid,
            );
        }
        if actual_pid == target_pid {
            ENUM_HWND.store(hwnd, std::sync::atomic::Ordering::SeqCst);
            return 0;
        }
        1
    }
    wm::EnumWindows(Some(enum_callback), pid as isize);
    let hwnd = ENUM_HWND.load(std::sync::atomic::Ordering::SeqCst);
    if !hwnd.is_null() { Some(hwnd) } else { None }
}

#[tauri::command]
fn click_inactive_window(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::UI::WindowsAndMessaging as wm;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse as km;
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use std::mem::{zeroed, size_of};
    use std::thread::sleep;
    use std::time::Duration;

    unsafe {
        let hwnd = find_hwnd_by_pid(pid).ok_or_else(|| "Window not found".to_string())?;

        let current_fg = wm::GetForegroundWindow();

        let mut wp: wm::WINDOWPLACEMENT = zeroed();
        wp.length = size_of::<wm::WINDOWPLACEMENT>() as u32;
        wm::GetWindowPlacement(hwnd, &mut wp);
        let was_minimized = wp.showCmd == wm::SW_SHOWMINIMIZED as u32;

        // Restore if minimized.
        if was_minimized {
            wm::ShowWindow(hwnd, wm::SW_RESTORE);
            sleep(Duration::from_millis(50));
        }

        // Send a short ALT press to satisfy foreground activation rules.
        let mut alt: km::INPUT = zeroed();
        alt.r#type = km::INPUT_KEYBOARD;
        alt.Anonymous.ki.wVk = km::VK_MENU;
        km::SendInput(1, &alt, size_of::<km::INPUT>() as i32);
        sleep(Duration::from_millis(10));
        let mut alt_up: km::INPUT = zeroed();
        alt_up.r#type = km::INPUT_KEYBOARD;
        alt_up.Anonymous.ki.wVk = km::VK_MENU;
        alt_up.Anonymous.ki.dwFlags = km::KEYEVENTF_KEYUP;
        km::SendInput(1, &alt_up, size_of::<km::INPUT>() as i32);
        sleep(Duration::from_millis(10));

        // Try to activate reliably and verify foreground ownership.
        let mut activated = false;
        for _ in 0..10 {
            wm::ShowWindow(hwnd, wm::SW_SHOW);
            wm::BringWindowToTop(hwnd);
            wm::SetForegroundWindow(hwnd);
            sleep(Duration::from_millis(30));
            if wm::GetForegroundWindow() == hwnd {
                activated = true;
                break;
            }
        }
        if !activated {
            return Err("Failed to activate target window".to_string());
        }

        // Move cursor to window center so click lands on target window.
        let mut prev_cursor = POINT { x: 0, y: 0 };
        wm::GetCursorPos(&mut prev_cursor);
        let mut rect: RECT = zeroed();
        if wm::GetWindowRect(hwnd, &mut rect) != 0 {
            let center_x = (rect.left + rect.right) / 2;
            let center_y = (rect.top + rect.bottom) / 2;
            wm::SetCursorPos(center_x, center_y);
            sleep(Duration::from_millis(20));
        }

        // Click once.
        let mut down: km::INPUT = zeroed();
        down.r#type = km::INPUT_MOUSE;
        down.Anonymous.mi.dwFlags = km::MOUSEEVENTF_LEFTDOWN;
        km::SendInput(1, &down, size_of::<km::INPUT>() as i32);
        sleep(Duration::from_millis(20));

        let mut up: km::INPUT = zeroed();
        up.r#type = km::INPUT_MOUSE;
        up.Anonymous.mi.dwFlags = km::MOUSEEVENTF_LEFTUP;
        km::SendInput(1, &up, size_of::<km::INPUT>() as i32);
        sleep(Duration::from_millis(10));
        wm::SetCursorPos(prev_cursor.x, prev_cursor.y);

        // Restore previous foreground window.
        if !current_fg.is_null() && current_fg != hwnd {
            wm::SetForegroundWindow(current_fg);
        }

        // Re-minimize only when the target was originally minimized.
        if was_minimized {
            wm::ShowWindow(hwnd, wm::SW_MINIMIZE);
        }
    }
    Ok(())
}
#[tauri::command]
async fn pick_vrchat_log_file() -> Result<Option<String>, String> {
    let default_path = get_default_vrchat_log_path_impl()
        .unwrap_or_else(|| "C:\\Users\\<Username>\\AppData\\LocalLow\\VRChat\\VRChat".to_string());

    let file = rfd::AsyncFileDialog::new()
        .add_filter("Log Files", &["txt"])
        .set_directory(&default_path)
        .pick_file()
        .await;

    Ok(file.map(|f| f.path().to_string_lossy().to_string()))
}

