const { invoke } = window.__TAURI__.core;

let itemList;
let addButton;
let debugButton;
let itemCount;
let vrchatInstances = [];
let isEacActive = false;

// helper to call tauri invoke
async function tauriInvoke(cmd, args) {
  return window.__TAURI__.core.invoke(cmd, args);
}

// VRChatインスタンスのデータ構造:
// {
//   profile: number,                // プロファイル番号 (1, 2, 3...)
//   name: string,                   // 表示名
//   status: string,                 // 'stopped' | 'launching' | 'running'
//   processId: number,              // プロセスID (実行中の場合)
//   waitingForMainProcess: boolean  // EACランチャー起動中で本体待機中かどうか
// }

function updateItemCount() {
  itemCount.textContent = `Instances: ${vrchatInstances.length}`;
}

function addLogEntry(message) {
  // Minimal logging to console instead of UI
  const timestamp = new Date().toLocaleTimeString();
  console.log(`[${timestamp}] ${message}`);
}

function addVRChatProfile() {
  // 次に利用可能なプロファイル番号を見つける
  const existingProfiles = vrchatInstances.map(instance => instance.profile);
  let nextProfile = 1;
  while (existingProfiles.includes(nextProfile)) {
    nextProfile++;
  }

  const newInstance = {
    profile: nextProfile,
    name: `VRChat Profile ${nextProfile}`,
    status: 'stopped',
    processId: null,
    waitingForMainProcess: false
  };

  vrchatInstances.push(newInstance);
  renderList();
  updateItemCount();
  addLogEntry(`新しいVRChatプロファイルを追加: Profile ${nextProfile}`);
}

function createListItem(instance, index) {
  const li = document.createElement('li');
  li.className = 'list-item';

  // ステータスに応じた表示色を設定
  let statusClass, statusText, pidText;
  if (instance.status === 'launching') {
    statusClass = 'status-launching';
    statusText = '起動中...';
    pidText = '';
  } else if (instance.status === 'running') {
    statusClass = 'status-running';
    statusText = '実行中';
    pidText = instance.processId ? ` (PID: ${instance.processId})` : '';
  } else {
    statusClass = 'status-stopped';
    statusText = '停止中';
    pidText = '';
  }

  // ボタンラベルと状態
  let btnLabel = 'Open';
  let btnDisabled = false;
  let btnTitle = '';

  if (instance.status === 'stopped') {
    btnLabel = 'Open';
    // Open は EAC が動作中のとき無効にする
    btnDisabled = isEacActive;
    btnTitle = isEacActive ? 'EACランチャーが起動中のためOpenできません' : '';
  } else if (instance.status === 'launching') {
    btnLabel = 'Waiting';
    btnDisabled = true; // Waiting は押せない
    btnTitle = 'ランチャー実行中です。起動を待っています';
  } else if (instance.status === 'running') {
    btnLabel = 'Close';
    btnDisabled = false;
    btnTitle = '';
  }

  li.innerHTML = `
    <span class="list-item-number">P${instance.profile}</span>
    <span class="list-item-content">
      <div class="instance-name">${instance.name}</div>
      <div class="instance-status ${statusClass}">${statusText}${pidText}</div>
    </span>
    <div class="instance-controls">
      <button class="setting-button" onclick="openSetting(${index})" title="Setting">Setting</button>
      <button class="toggle-button" onclick="toggleInstance(${index})" ${btnDisabled ? 'disabled' : ''} title="${btnTitle}">${btnLabel}</button>
      <button class="delete-button" onclick="removeInstance(${index})">×</button>
    </div>
  `;
  return li;
}

// Toggle helper: Open -> launch, Waiting -> disabled, Close -> stop
async function toggleInstance(index) {
  const instance = vrchatInstances[index];
  if (!instance) return;

  if (instance.status === 'stopped') {
    // attempt to open
    await openVRChat(index);
  } else if (instance.status === 'running') {
    // attempt to close
    await closeVRChat(index);
  }
  // if launching, button is disabled and shouldn't trigger
}

function removeInstance(index) {
  const removedInstance = vrchatInstances[index];

  // 実行中の場合は警告
  if (removedInstance.status === 'running') {
    if (!confirm(`Profile ${removedInstance.profile} は実行中です。停止してから削除しますか？`)) {
      return;
    }
    // 必要に応じてプロセスを終了する処理をここに追加
  }

  vrchatInstances.splice(index, 1);
  renderList();
  updateItemCount();
  addLogEntry(`VRChatプロファイルを削除: Profile ${removedInstance.profile}`);
}

// VRChatインスタンスを開く
async function openVRChat(index) {
  const instance = vrchatInstances[index];
  if (instance.status !== 'stopped') {
    addLogEntry(`Profile ${instance.profile} は既に起動中または起動処理中です`);
    return;
  }

  try {
    addLogEntry(`VRChat Profile ${instance.profile} を起動中...`);

    // 起動する前にEACランチャーが既に起動していないか再確認
    const eacActive = await tauriInvoke('is_eac_launcher_running');
    if (eacActive) {
      addLogEntry('❌ EACランチャーが既に動作中です。Open を中止しました');
      return;
    }

    const result = await tauriInvoke('launch_vrchat', { profile: instance.profile });

    if (result.success) {
      if (result.waiting_for_main_process) {
        instance.status = 'launching';
        instance.waitingForMainProcess = true;
      } else {
        instance.status = 'running';
      }
      instance.processId = result.process_id;
      renderList();
      addLogEntry(`✅ ${result.message} (PID: ${result.process_id})`);
    } else {
      addLogEntry(`❌ ${result.message}`);
    }
  } catch (error) {
    addLogEntry(`❌ VRChat Profile ${instance.profile} の起動に失敗: ${error}`);
  }
}

// VRChatインスタンスを閉じる
async function closeVRChat(index) {
  const instance = vrchatInstances[index];
  if (instance.status === 'stopped') {
    addLogEntry(`Profile ${instance.profile} は既に停止中です`);
    return;
  }

  try {
    addLogEntry(`VRChat Profile ${instance.profile} を停止中...`);

    const result = await tauriInvoke('stop_vrchat', { profile: instance.profile });

    if (result.success) {
      instance.status = 'stopped';
      const oldPid = instance.processId;
      instance.processId = null;
      renderList();
      addLogEntry(`✅ ${result.message} (PID: ${oldPid})`);
    } else {
      addLogEntry(`❌ ${result.message}`);
    }
  } catch (error) {
    addLogEntry(`❌ VRChat Profile ${instance.profile} の停止に失敗: ${error}`);
  }
}

// Open the Mining Setting sub-window via Tauri command
async function openSetting(index) {
  try {
    await tauriInvoke('create_sub_window');
    addLogEntry(`Setting ウィンドウを開きました (Profile ${vrchatInstances[index].profile})`);
  } catch (error) {
    addLogEntry(`Setting ウィンドウの作成に失敗: ${error}`);
  }
}

function renderList() {
  itemList.innerHTML = '';
  vrchatInstances.forEach((instance, index) => {
    const listItem = createListItem(instance, index);
    itemList.appendChild(listItem);
  });
}

// グローバル関数（onclick用）
window.removeInstance = removeInstance;
window.toggleInstance = toggleInstance;
window.openSetting = openSetting;

// 実行中のVRChatプロセス状態をチェック（簡素化）
async function checkRunningProcesses() {
  try {
    const runningProcesses = await tauriInvoke('get_running_vrchat');

    // 各インスタンスの状態を更新
    let statusChanged = false;
    vrchatInstances.forEach(instance => {
      const currentPid = runningProcesses[instance.profile];
      const isRunning = currentPid !== undefined;

      // 起動中 (launching) の場合
      if (instance.status === 'launching') {
        if (isRunning) {
          // VRChat.exe本体が起動した
          instance.status = 'running';
          instance.processId = currentPid;
          instance.waitingForMainProcess = false;
          statusChanged = true;
          addLogEntry(`✅ VRChat Profile ${instance.profile} 本体起動完了 (PID: ${currentPid})`);
        }
        // まだ起動していない場合は launching 状態を維持
      }
      // 実行中の場合
      else if (instance.status === 'running') {
        if (!isRunning) {
          // プロセスが停止した
          instance.status = 'stopped';
          instance.processId = null;
          instance.waitingForMainProcess = false;
          statusChanged = true;
          addLogEntry(`⚠️ VRChat Profile ${instance.profile} が停止しました`);
        } else if (instance.processId !== currentPid) {
          // プロセスIDが変更された
          const oldPid = instance.processId;
          instance.processId = currentPid;
          statusChanged = true;
          addLogEntry(`🔄 VRChat Profile ${instance.profile} プロセス移行: ${oldPid} → ${currentPid}`);
        }
      }
      // 停止中の場合
      else if (instance.status === 'stopped') {
        if (isRunning) {
          // 外部から起動された
          instance.status = 'running';
          instance.processId = currentPid;
          instance.waitingForMainProcess = false;
          statusChanged = true;
          addLogEntry(`✅ VRChat Profile ${instance.profile} が検出されました (PID: ${currentPid})`);
        }
      }
    });

    // 未知のVRChatプロセスを検出（外部起動）
    const knownProfiles = new Set(vrchatInstances.map(i => i.profile));
    for (const [profileStr, pid] of Object.entries(runningProcesses)) {
      const profile = parseInt(profileStr);
      if (!knownProfiles.has(profile)) {
        addLogEntry(`🆕 未知のVRChat Profile ${profile} を検出 (PID: ${pid}) - 手動追加をお勧めします`);
      }
    }

    if (statusChanged) {
      renderList();
    }
  } catch (error) {
    console.error('プロセス状態チェックエラー:', error);
  }
}

// EACランチャーが起動しているかどうかを定期チェックし、Openボタンを無効化する
async function checkEacLauncher() {
  try {
    const active = await tauriInvoke('is_eac_launcher_running');
    if (active !== isEacActive) {
      isEacActive = active;
      addLogEntry(isEacActive ? '⚠️ EACランチャーが検出されました。Openボタンを無効化します' : '✅ EACランチャーが見つかりません。Openボタンを有効化します');
      renderList();
    }
  } catch (error) {
    console.error('EACチェックエラー:', error);
  }
}

// debug 機能は削除（運用で不要になったため）

window.addEventListener("DOMContentLoaded", () => {
  itemList = document.querySelector("#instance_list");
  addButton = document.querySelector("#add-button");
  itemCount = document.querySelector("#item-count");

  // ボタンイベントリスナー
  addButton.addEventListener("click", addVRChatProfile);

  // 初期状態では空のリストから開始
  vrchatInstances = [];
  renderList();
  updateItemCount();
  
  // 初期ログエントリ
  addLogEntry("VRChat Manager が初期化されました");
  addLogEntry("'Add' ボタンで新しいVRChatプロファイルを追加できます");
  addLogEntry("VRChatパス: C:\\Program Files (x86)\\Steam\\steamapps\\common\\VRChat\\start_protected_game.exe");
  
  // 3秒毎にプロセス状態をチェック（頻度を上げる）
  setInterval(checkRunningProcesses, 3000);
  // 2秒毎にEACランチャーの存在をチェック
  setInterval(checkEacLauncher, 2000);
  // 初回チェック
  setTimeout(checkEacLauncher, 500);
  
  // 初回チェック
  setTimeout(checkRunningProcesses, 1000);
});
