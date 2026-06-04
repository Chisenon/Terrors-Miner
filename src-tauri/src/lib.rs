use std::process::Command;
use std::collections::{HashMap, VecDeque, HashSet};
use std::sync::Mutex;
use std::io::Read;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sysinfo::{System, Pid};
use std::thread;
use std::time::Duration;
use base64::Engine;
use flate2::read::GzDecoder;

use once_cell::sync::Lazy;
static VRCHAT_PROCESSES: Lazy<Mutex<HashMap<u32, u32>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static PENDING_PROFILES: Lazy<Mutex<VecDeque<u32>>> = Lazy::new(|| Mutex::new(VecDeque::new()));
static MISSED_DETECTIONS: Lazy<Mutex<HashMap<u32, u32>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static STOPPING_PROFILES: Lazy<Mutex<HashSet<u32>>> = Lazy::new(|| Mutex::new(HashSet::new()));

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
        .args(&["--no-vr", &format!("--profile={}", profile)])
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
                            }
                        } else {
                            eprintln!("[PID MONITOR] Profile {} registered: PID {}", profile, new_pid);
                            stored.insert(profile, new_pid);
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
                                assigned = true;
                                break;
                            }
                            if !assigned {
                                let used: HashSet<u32> = stored.keys().copied().collect();
                                let next_profile = (1..).find(|n| !used.contains(n)).unwrap_or(1);
                                if !stopping_snapshot.contains(&next_profile) {
                                    eprintln!("[PID MONITOR] Auto-assigned Profile {} to PID {} (no --profile)", next_profile, new_pid);
                                    stored.insert(next_profile, new_pid);
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    spawn_vrchat_pid_monitor();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            launch_vrchat,
            stop_vrchat,
            get_running_vrchat,
            debug_vrchat_processes,
            is_eac_launcher_running,
            create_sub_window,
            decode_tracker_import
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
async fn create_sub_window(app: tauri::AppHandle) -> Result<(), String> {
    let label = "Mining_Setting";
    match tauri::WebviewWindowBuilder::new(&app, label, tauri::WebviewUrl::App("mining_setting.html".into()))
        .title("Mining Setting")
        .inner_size(600.0, 400.0)
        .build()
    {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Failed to create sub-window: {}", e)),
    }
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
