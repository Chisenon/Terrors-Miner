use std::process::Command;
use std::collections::{HashMap, VecDeque, HashSet};
use std::sync::Mutex;
use serde::{Deserialize, Serialize};
use sysinfo::{System, Pid};
use std::thread;
use std::time::Duration;

// VRChatプロセス管理用のグローバル状態
use once_cell::sync::Lazy;
static VRCHAT_PROCESSES: Lazy<Mutex<HashMap<u32, u32>>> = Lazy::new(|| Mutex::new(HashMap::new()));

// 最近Openボタンが押されたプロファイルをFIFOで記録（次に検出された未知のVRChat PIDに順番に割り当てる）
// Vec<u32> をキューとして使用（先入れ先出し）
static PENDING_PROFILES: Lazy<Mutex<VecDeque<u32>>> = Lazy::new(|| Mutex::new(VecDeque::new()));

// 連続で検出されなかった回数をカウントして安定化（急なフラップを避ける）
// profile -> missed_count
static MISSED_DETECTIONS: Lazy<Mutex<HashMap<u32, u32>>> = Lazy::new(|| Mutex::new(HashMap::new()));

// 現在停止処理中のプロファイル（監視ループがそのプロファイルを操作しないようにするため）
static STOPPING_PROFILES: Lazy<Mutex<HashSet<u32>>> = Lazy::new(|| Mutex::new(HashSet::new()));

// RAII guard: mark a profile as 'stopping' while this guard is alive
struct StopGuard {
    profile: u32,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        let mut s = STOPPING_PROFILES.lock().unwrap();
        s.remove(&self.profile);
        eprintln!("[STOP GUARD] Profile {} 停止フラグ解除", self.profile);
    }
}

#[derive(Serialize, Deserialize)]
struct VRChatResult {
    success: bool,
    message: String,
    process_id: Option<u32>,
    // EACランチャー起動中で本体のVRChat.exeを待機している状態かどうか
    waiting_for_main_process: Option<bool>,
}

// VRChatを起動するTauriコマンド
#[tauri::command]
async fn launch_vrchat(profile: u32) -> Result<VRChatResult, String> {
    let vrchat_path = r"C:\Program Files (x86)\Steam\steamapps\common\VRChat\start_protected_game.exe";
    
    // プロセスが既に実行中かチェック
    {
        let processes = VRCHAT_PROCESSES.lock().unwrap();
        if let Some(&existing_pid) = processes.get(&profile) {
            // プロセスがまだ存在するかチェック
            let mut system = System::new_all();
            system.refresh_all();
            if system.process(Pid::from(existing_pid as usize)).is_some() {
                return Ok(VRChatResult {
                    success: false,
                    message: format!("Profile {} は既に実行中です (PID: {})", profile, existing_pid),
                    process_id: Some(existing_pid),
                    waiting_for_main_process: Some(false),
                });
            }
        }
    }

    // VRChatプロセスを起動（start_protected_game.exeが起動し、それがVRChat.exeを起動する）
    match Command::new(vrchat_path)
        .args(&["--no-vr", &format!("--profile={}", profile)])
        .spawn()
    {
        Ok(child) => {
            let launcher_pid = child.id();
            
            // start_protected_game.exeのPIDは記録しない
            // バックグラウンド監視が実際のVRChat.exeを検出するまで待つ
            eprintln!("[LAUNCH] Profile {} EACランチャー起動 (PID: {}) → VRChat.exe起動待機中", profile, launcher_pid);
            
            // このプロファイルをキューに追加（次に検出される未知のVRChat PIDに順次割り当てる）
            {
                let mut pending = PENDING_PROFILES.lock().unwrap();
                pending.push_back(profile);
                eprintln!("[LAUNCH] Profile {} を待機キューに追加（次に検出される未知のVRChat PID と関連付け）", profile);
            }
            
            Ok(VRChatResult {
                success: true,
                message: format!("VRChat Profile {} を起動しました（本体の起動を監視中...）", profile),
                process_id: Some(launcher_pid),
                waiting_for_main_process: Some(true),
            })
        }
        Err(e) => Ok(VRChatResult {
            success: false,
            message: format!("VRChatの起動に失敗しました: {}", e),
            process_id: None,
            waiting_for_main_process: Some(false),
        }),
    }
}

// VRChatプロセスを停止するTauriコマンド
#[tauri::command]
async fn stop_vrchat(profile: u32) -> Result<VRChatResult, String> {
    // Try to stop the VRChat process for the given profile.
    // Strategy:
    // 1) Check stored mapping for the profile and try to kill that PID if it looks like VRChat
    // 2) If not found or doesn't look like VRChat, scan all processes for a process whose
    //    command line contains "--profile=<profile>" and kill it.
    // 3) Remove stored mapping if present.

    // Remove profile from pending queue (if any)
    {
        let mut pending = PENDING_PROFILES.lock().unwrap();
        pending.retain(|p| *p != profile);
    }

    // Mark profile as stopping so monitor doesn't race on it
    {
        let mut s = STOPPING_PROFILES.lock().unwrap();
        s.insert(profile);
        eprintln!("[STOP GUARD] Profile {} 停止フラグセット", profile);
    }
    // create guard that will remove the flag when this function returns
    let _stop_guard = StopGuard { profile };

    let mut system = System::new_all();
    system.refresh_all();

    // Get stored pid (if any) without removing yet
    let stored_pid = {
        let processes = VRCHAT_PROCESSES.lock().unwrap();
        processes.get(&profile).copied()
    };

    // helper to kill a pid and return bool success
    fn kill_and_wait(pid: u32) -> bool {
        // attempt to kill and wait for process to disappear (short polling)
        let mut s = System::new_all();
        s.refresh_all();
        if let Some(process) = s.process(Pid::from(pid as usize)) {
            let _ = process.kill();
        }
        // wait up to ~1s (5 * 200ms)
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(200));
            s.refresh_all();
            if s.process(Pid::from(pid as usize)).is_none() {
                return true;
            }
        }
        false
    }

    // 1) try stored pid
    if let Some(pid) = stored_pid {
        // refresh and check exe/name for vrchat
        system.refresh_all();
        if let Some(process) = system.process(Pid::from(pid as usize)) {
            let name = process.name().to_lowercase();
            let exe = process.exe().map(|p| p.to_string_lossy().to_string().to_lowercase()).unwrap_or_default();
            if name.contains("vrchat") || exe.contains("vrchat") {
                // attempt kill and wait
                if kill_and_wait(pid) {
                    // remove mapping
                    let mut processes = VRCHAT_PROCESSES.lock().unwrap();
                    processes.remove(&profile);
                    return Ok(VRChatResult {
                        success: true,
                        message: format!("VRChat Profile {} を停止しました", profile),
                        process_id: Some(pid),
                        waiting_for_main_process: Some(false),
                    });
                } else {
                    return Ok(VRChatResult {
                        success: false,
                        message: format!("Profile {} のプロセスの強制終了に失敗しました (PID: {})", profile, pid),
                        process_id: Some(pid),
                        waiting_for_main_process: Some(false),
                    });
                }
            }
            // otherwise fallthrough to scanning by cmdline
        }
    }

    // 2) scan for process with --profile=<n> in its cmdline
    let mut found_pid: Option<u32> = None;
    for (pid, process) in system.processes() {
        let cmd_line = process.cmd().join(" ");
        if cmd_line.contains(&format!("--profile={}", profile)) {
            found_pid = Some(pid.as_u32());
            break;
        }
    }

    if let Some(pid) = found_pid {
        // attempt kill and wait for exit
        if kill_and_wait(pid) {
            let mut processes = VRCHAT_PROCESSES.lock().unwrap();
            // ensure removed even if mapping absent
            processes.retain(|&p, &mut _| p != profile);
            processes.remove(&profile);
            return Ok(VRChatResult {
                success: true,
                message: format!("VRChat Profile {} を停止しました (PID: {})", profile, pid),
                process_id: Some(pid),
                waiting_for_main_process: Some(false),
            });
        } else {
            return Ok(VRChatResult {
                success: false,
                message: format!("Profile {} のプロセスの強制終了に失敗しました (PID: {})", profile, pid),
                process_id: Some(pid),
                waiting_for_main_process: Some(false),
            });
        }
    }

    // 3) nothing found/killed
    Ok(VRChatResult {
        success: false,
        message: format!("Profile {} のプロセスが見つかりません", profile),
        process_id: None,
        waiting_for_main_process: Some(false),
    })

}

// 実行中のVRChatプロセスを取得するTauriコマンド
#[tauri::command]
async fn get_running_vrchat() -> Result<HashMap<u32, u32>, String> {
    let mut result = HashMap::new();
    let mut system = System::new_all();
    system.refresh_all();
    
    let processes = VRCHAT_PROCESSES.lock().unwrap();
    for (&profile, &pid) in processes.iter() {
        // プロセスがまだ存在するかチェック
        if system.process(Pid::from(pid as usize)).is_some() {
            result.insert(profile, pid);
        }
    }
    
    Ok(result)
}

// デバッグ用: VRChat関連のプロセスを検出
#[tauri::command]
async fn debug_vrchat_processes() -> Result<Vec<String>, String> {
    let mut system = System::new_all();
    system.refresh_all();
    let mut debug_info = Vec::new();
    
    // VRChat関連のプロセスを検索
    for (pid, process) in system.processes() {
        let name = process.name().to_string();
        let name_lower = name.to_lowercase();
        let exe_path = process.exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        
        // VRChat関連のプロセスを検出（より広範囲で検索）
        if name_lower.contains("vrchat") || 
           exe_path.contains("vrchat") || 
           name_lower.contains("start_protected_game") ||
           name_lower.contains("easyanticheat") ||
           name_lower.contains("eac") {
            let cmd_line = process.cmd().join(" ");
            let parent_pid = process.parent().map(|p| p.as_u32()).unwrap_or(0);
            let start_time = process.start_time();
            
            // プロファイル番号を抽出
            let profile = extract_profile_from_cmd(&cmd_line);
            let profile_str = profile.map(|p| format!("Profile={}", p)).unwrap_or_else(|| "Profile=なし".to_string());
            
            debug_info.push(format!(
                "PID: {}, Name: {}, {}, Parent: {}, StartTime: {}, Exe: {}, Args: {}",
                pid, name, profile_str, parent_pid, start_time, exe_path, cmd_line
            ));
        }
    }
    
    // 時刻順にソート
    debug_info.sort();
    Ok(debug_info)
}

// コマンドラインからプロファイル番号を抽出
fn extract_profile_from_cmd(cmd_line: &str) -> Option<u32> {
    // "--profile=1" のような形式を検索
    if let Some(start) = cmd_line.find("--profile=") {
        let profile_str = &cmd_line[start + 10..];
        if let Some(end) = profile_str.find(' ').or(Some(profile_str.len())) {
            let profile_num_str = &profile_str[..end];
            return profile_num_str.parse().ok();
        }
    }
    None
}

// バックグラウンドでVRChatのPIDを監視し、PIDが変わったらログに出力して記録を更新する
// EAC対応: 初回検出はスキップし、2回目の検出から記録開始
fn spawn_vrchat_pid_monitor() {
    const INTERVAL_SECONDS: u64 = 3;

    thread::spawn(|| {
        loop {
            let mut system = System::new_all();
            system.refresh_all();

            // 現在検出されているVRChatプロセス
            let mut detected_processes = Vec::new();
            for (pid, process) in system.processes() {
                let name = process.name().to_string();
                let name_lower = name.to_lowercase();
                
                // VRChat.exeを検出（大文字小文字を区別しない、start_protected_game.exeは除外）
                if (name_lower.contains("vrchat.exe") || name_lower == "vrchat.exe") && 
                   !name_lower.contains("start_protected_game") {
                    let cmd_line = process.cmd().join(" ");
                        // 検出ログは冗長になりがちなので詳細ログはデバッグコマンドに任せる
                        let profile = extract_profile_from_cmd(&cmd_line);
                    detected_processes.push((pid.as_u32(), profile));
                }
            }

            // シンプルなプロセス管理
            {
                let mut stored = VRCHAT_PROCESSES.lock().unwrap();

                // 停止検出とクリーンアップ（安定化のために連続未検出を2回許容）
                let detected_pids: Vec<u32> = detected_processes.iter().map(|(pid, _)| *pid).collect();

                // For each stored profile, if its pid is not detected increment missed count;
                // if detected, reset missed count to 0.
                {
                    let mut missed = MISSED_DETECTIONS.lock().unwrap();
                    let mut to_remove = Vec::new();

                    // snapshot stopping profiles to avoid holding two locks in inner loop
                    let stopping_snapshot = STOPPING_PROFILES.lock().unwrap().clone();

                    for (&profile, &pid) in stored.iter() {
                        // if a profile is currently being stopped, skip detection/removal to avoid races
                        if stopping_snapshot.contains(&profile) {
                            continue;
                        }

                        if !detected_pids.contains(&pid) {
                            let cnt = missed.entry(profile).or_insert(0);
                            *cnt += 1;
                            // require 2 consecutive misses before treating as stopped
                            if *cnt >= 2 {
                                to_remove.push((profile, pid));
                            }
                        } else {
                            // seen -> reset counter
                            missed.remove(&profile);
                        }
                    }

                    for (profile, old_pid) in to_remove {
                        if let Some(removed_pid) = stored.remove(&profile) {
                            eprintln!("[PID MONITOR] Profile {} PID {} 連続未検出 -> 記録削除", profile, removed_pid);
                        }
                        missed.remove(&profile);
                    }
                }

                // 新規/変更検出
                // 注: detected_processesはVRChat.exeのみ（start_protected_game.exeは除外済み）
                // snapshot stopping profiles again for assignment checks
                let stopping_snapshot = STOPPING_PROFILES.lock().unwrap().clone();

                for (new_pid, cmd_profile) in detected_processes {
                    if let Some(profile) = cmd_profile {
                        // --profile=N が指定されている場合
                        if stopping_snapshot.contains(&profile) {
                            eprintln!("[PID MONITOR] Profile {} は停止中のため検出を無視: PID {}", profile, new_pid);
                            continue;
                        }

                        if let Some(&old_pid) = stored.get(&profile) {
                            if old_pid != new_pid {
                                // PIDが変更された（ログは簡潔に）
                                eprintln!("[PID MONITOR] Profile {} PID変化: {} -> {}", profile, old_pid, new_pid);
                                stored.insert(profile, new_pid);
                            }
                        } else {
                            // 新規プロファイル検出（VRChat.exeが起動した）
                            eprintln!("[PID MONITOR] Profile {} 登録: PID {}", profile, new_pid);
                            stored.insert(profile, new_pid);
                        }
                    } else {
                        // --profile= が指定されていない場合
                        // このPIDが既存のどのプロファイルにも登録されていないか確認
                        let already_stored = stored.values().any(|&pid| pid == new_pid);
                        if !already_stored {
                            eprintln!("[PID MONITOR] ⚠️ プロファイル番号なしのVRChat.exe検出 PID: {} (コマンドライン引数に --profile= がありません)", new_pid);

                            // 待機キューから次のプロファイルを取り出して関連付ける（FIFO）
                            let mut pending = PENDING_PROFILES.lock().unwrap();
                            // pop until we find a profile that's not currently stopping
                            let mut assigned = false;
                            while let Some(profile) = pending.pop_front() {
                                if stopping_snapshot.contains(&profile) {
                                    eprintln!("[PID MONITOR] 待機キューから Profile {} をスキップ（停止中）", profile);
                                    continue;
                                }
                                eprintln!("[PID MONITOR] ✅ 待機キューから Profile {} を取り出して VRChat.exe PID {} を関連付け", profile, new_pid);
                                stored.insert(profile, new_pid);
                                assigned = true;
                                break;
                            }
                            if !assigned {
                                eprintln!("[PID MONITOR] 待機キューに停止中のプロファイルしかないか空のため自動関連付けなし");
                            }
                        }
                    }
                }
            }

            thread::sleep(Duration::from_secs(INTERVAL_SECONDS));
        }
    });
}

// 既存のgreetコマンド
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // start background PID monitor
    spawn_vrchat_pid_monitor();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            launch_vrchat,
            stop_vrchat,
            get_running_vrchat,
            debug_vrchat_processes,
            is_eac_launcher_running
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// EACランチャー（start_protected_game.exe）が実行中かどうかを返す
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
