const { invoke } = window.__TAURI__.core;

let itemList;
let addButton;
let itemCount;
let vrchatInstances = [];
let isEacActive = false;

async function tauriInvoke(cmd, args) {
  return window.__TAURI__.core.invoke(cmd, args);
}

function updateItemCount() {
  itemCount.textContent = `Instances: ${vrchatInstances.length}`;
}

function addLogEntry(message) {
  const timestamp = new Date().toLocaleTimeString();
  console.log(`[${timestamp}] ${message}`);
}

function addVRChatProfile() {
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
  addLogEntry(`New VRChat profile added: Profile ${nextProfile}`);
}

function createListItem(instance, index) {
  const li = document.createElement('li');
  li.className = 'list-item';

  let statusClass, statusText, pidText;
  if (instance.status === 'launching') {
    statusClass = 'status-launching';
    statusText = 'Launching...';
    pidText = '';
  } else if (instance.status === 'running') {
    statusClass = 'status-running';
    statusText = 'Running';
    pidText = instance.processId ? ` (PID: ${instance.processId})` : '';
  } else {
    statusClass = 'status-stopped';
    statusText = 'Stopped';
    pidText = '';
  }

  let btnLabel = 'Open';
  let btnDisabled = false;
  let btnTitle = '';

  if (instance.status === 'stopped') {
    btnLabel = 'Open';
    btnDisabled = isEacActive;
    btnTitle = isEacActive ? 'Cannot Open while EAC launcher is running' : '';
  } else if (instance.status === 'launching') {
    btnLabel = 'Waiting';
    btnDisabled = true;
    btnTitle = 'Launcher is running, waiting for VRChat';
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

async function toggleInstance(index) {
  const instance = vrchatInstances[index];
  if (!instance) return;

  if (instance.status === 'stopped') {
    await openVRChat(index);
  } else if (instance.status === 'running') {
    await closeVRChat(index);
  }
}

function removeInstance(index) {
  const removedInstance = vrchatInstances[index];

  if (removedInstance.status === 'running') {
    if (!confirm(`Profile ${removedInstance.profile} is running. Stop and delete?`)) {
      return;
    }
  }

  vrchatInstances.splice(index, 1);
  renderList();
  updateItemCount();
  addLogEntry(`Profile deleted: Profile ${removedInstance.profile}`);
}

async function openVRChat(index) {
  const instance = vrchatInstances[index];
  if (instance.status !== 'stopped') {
    addLogEntry(`Profile ${instance.profile} is already starting or running`);
    return;
  }

  try {
    addLogEntry(`Starting VRChat Profile ${instance.profile}...`);

    const eacActive = await tauriInvoke('is_eac_launcher_running');
    if (eacActive) {
      addLogEntry('❌ EAC launcher is already running. Open aborted');
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
    addLogEntry(`❌ Failed to launch VRChat Profile ${instance.profile}: ${error}`);
  }
}

async function closeVRChat(index) {
  const instance = vrchatInstances[index];
  if (instance.status === 'stopped') {
    addLogEntry(`Profile ${instance.profile} is already stopped`);
    return;
  }

  try {
    addLogEntry(`Stopping VRChat Profile ${instance.profile}...`);

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
    addLogEntry(`❌ Failed to stop VRChat Profile ${instance.profile}: ${error}`);
  }
}

async function openSetting(index) {
  try {
    await tauriInvoke('create_sub_window');
    addLogEntry(`Setting window opened (Profile ${vrchatInstances[index].profile})`);
  } catch (error) {
    addLogEntry(`Failed to create Setting window: ${error}`);
  }
}

function renderList() {
  itemList.innerHTML = '';
  vrchatInstances.forEach((instance, index) => {
    const listItem = createListItem(instance, index);
    itemList.appendChild(listItem);
  });
}

window.removeInstance = removeInstance;
window.toggleInstance = toggleInstance;
window.openSetting = openSetting;

async function checkRunningProcesses() {
  try {
    const runningProcesses = await tauriInvoke('get_running_vrchat');

    let statusChanged = false;
    vrchatInstances.forEach(instance => {
      const currentPid = runningProcesses[instance.profile];
      const isRunning = currentPid !== undefined;

      if (instance.status === 'launching') {
        if (isRunning) {
          instance.status = 'running';
          instance.processId = currentPid;
          instance.waitingForMainProcess = false;
          statusChanged = true;
          addLogEntry(`✅ VRChat Profile ${instance.profile} main process started (PID: ${currentPid})`);
        }
      }
      else if (instance.status === 'running') {
        if (!isRunning) {
          instance.status = 'stopped';
          instance.processId = null;
          instance.waitingForMainProcess = false;
          statusChanged = true;
          addLogEntry(`⚠️ VRChat Profile ${instance.profile} stopped`);
        } else if (instance.processId !== currentPid) {
          const oldPid = instance.processId;
          instance.processId = currentPid;
          statusChanged = true;
          addLogEntry(`🔄 VRChat Profile ${instance.profile} process changed: ${oldPid} → ${currentPid}`);
        }
      }
      else if (instance.status === 'stopped') {
        if (isRunning) {
          instance.status = 'running';
          instance.processId = currentPid;
          instance.waitingForMainProcess = false;
          statusChanged = true;
          addLogEntry(`✅ VRChat Profile ${instance.profile} detected (PID: ${currentPid})`);
        }
      }
    });

    const knownProfiles = new Set(vrchatInstances.map(i => i.profile));
    for (const [profileStr, pid] of Object.entries(runningProcesses)) {
      const profile = parseInt(profileStr);
      if (!knownProfiles.has(profile)) {
        const newInstance = {
          profile: profile,
          name: `VRChat Profile ${profile}`,
          status: 'running',
          processId: pid,
          waitingForMainProcess: false
        };
        vrchatInstances.push(newInstance);
        statusChanged = true;
        addLogEntry(`🆕 Existing VRChat Profile ${profile} auto-added (PID: ${pid})`);
      }
    }

    if (statusChanged) {
      renderList();
      updateItemCount();
    }
  } catch (error) {
    console.error('Process check error:', error);
  }
}

async function checkEacLauncher() {
  try {
    const active = await tauriInvoke('is_eac_launcher_running');
    if (active !== isEacActive) {
      isEacActive = active;
      addLogEntry(isEacActive ? '⚠️ EAC launcher detected. Open button disabled' : '✅ EAC launcher not found. Open button enabled');
      renderList();
    }
  } catch (error) {
    console.error('EAC check error:', error);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  itemList = document.querySelector("#instance_list");
  addButton = document.querySelector("#add-button");
  itemCount = document.querySelector("#item-count");

  addButton.addEventListener("click", addVRChatProfile);

  vrchatInstances = [];
  renderList();
  updateItemCount();

  addLogEntry("VRChat Manager initialized");
  addLogEntry("Click 'Add' to create a new VRChat profile");
  addLogEntry("VRChat path: C:\\Program Files (x86)\\Steam\\steamapps\\common\\VRChat\\start_protected_game.exe");

  setInterval(checkRunningProcesses, 3000);
  setInterval(checkEacLauncher, 2000);
  setTimeout(checkEacLauncher, 500);
  setTimeout(checkRunningProcesses, 1000);
});
