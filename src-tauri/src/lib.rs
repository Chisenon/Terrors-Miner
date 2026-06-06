use std::process::Command;
use std::collections::{HashMap, VecDeque, HashSet};
use std::sync::Mutex;
use std::io::Read;
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
static PROFILE_OSC_PORTS: Lazy<Mutex<HashMap<u32, OscPorts>>> = Lazy::new(|| Mutex::new(HashMap::new()));

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

async fn send_osc_left_click(profile: u32) -> Result<(), String> {
    // Desktop pickup/use is usually right-hand use in OSC.
    send_osc_int(profile, "/input/UseRight", 1).await?;
    sleep(Duration::from_millis(50)).await;
    send_osc_int(profile, "/input/UseRight", 0).await?;
    // Some interactions are grab-based; pulse grab as fallback.
    send_osc_int(profile, "/input/GrabRight", 1).await?;
    sleep(Duration::from_millis(50)).await;
    send_osc_int(profile, "/input/GrabRight", 0).await?;
    Ok(())
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
    sleep(Duration::from_millis(200)).await;
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
            get_default_vrchat_log_path,
            pick_vrchat_log_file,
            set_profile_log_file,
            get_profile_log_file
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
    Ok(())
}

#[tauri::command]
fn get_profile_log_file(profile: u32) -> Result<String, String> {
    let log_files = PROFILE_LOG_FILES.lock().unwrap();
    log_files.get(&profile).cloned().ok_or_else(|| "Not set".to_string())
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
