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

async function addVRChatProfile() {
  try {
    const attachResult = await tauriInvoke('attach_existing_vrchat');
    if (attachResult && attachResult.attached) {
      const attachedProfile = attachResult.profile;
      const attachedPid = attachResult.process_id;
      const existingIndex = vrchatInstances.findIndex(
        (instance) => instance.profile === attachedProfile || instance.processId === attachedPid
      );

      const attachedInstance = {
        profile: attachedProfile,
        name: `VRChat Existing PID ${attachedPid}`,
        status: 'running',
        processId: attachedPid,
        waitingForMainProcess: false,
        isExistingProcess: true,
        oscInPort: attachResult.in_port ?? null,
        oscOutPort: attachResult.out_port ?? null,
        autoActive: false,
        autoJumpTimerId: null
      };

      if (existingIndex >= 0) {
        vrchatInstances[existingIndex] = { ...vrchatInstances[existingIndex], ...attachedInstance };
      } else {
        vrchatInstances.push(attachedInstance);
      }

      renderList();
      updateItemCount();
      addLogEntry(`${attachResult.message} (OSC in:${attachResult.in_port} out:${attachResult.out_port})`);
      return;
    }
  } catch (error) {
    addLogEntry(`Failed to attach existing VRChat process: ${error}`);
  }

  const existingProfiles = vrchatInstances.map((instance) => instance.profile);
  let nextProfile = 1;
  while (existingProfiles.includes(nextProfile)) {
    nextProfile++;
  }

  const newInstance = {
    profile: nextProfile,
    name: `VRChat Profile ${nextProfile}`,
    status: 'stopped',
    processId: null,
    waitingForMainProcess: false,
    isExistingProcess: false,
    autoActive: false,
    autoJumpTimerId: null
  };

  vrchatInstances.push(newInstance);
  renderList();
  updateItemCount();
  addLogEntry(`New VRChat profile added: Profile ${nextProfile}`);
}

function createListItem(instance, index) {
  const li = document.createElement('li');
  li.className = 'list-item';
  const instanceLabel = instance.isExistingProcess && instance.processId
    ? `PID ${instance.processId}`
    : `P${instance.profile}`;

  let statusClass;
  let statusText;
  let pidText;
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
    <span class="list-item-number">${instanceLabel}</span>
    <span class="list-item-content">
      <div class="instance-name">${instance.name}</div>
      <div class="instance-status ${statusClass}">${statusText}${pidText}</div>
    </span>
    <div class="instance-controls">
      <button class="placeholder-button" type="button" disabled title="Start/Stop (placeholder)">Start/Stop</button>
      <button class="auto-button${instance.autoActive ? ' auto-active' : ''}" onclick="toggleAuto(${index})" ${instance.status !== 'running' ? 'disabled' : ''} title="${instance.autoActive ? 'Automation active' : 'Start automation'}">Auto</button>
      <button class="toggle-button" onclick="toggleInstance(${index})" ${btnDisabled ? 'disabled' : ''} title="${btnTitle}">${btnLabel}</button>
      <button class="setting-button" onclick="openSetting(${index})" title="Setting">&#9881;</button>
    </div>
    <button class="delete-button" onclick="removeInstance(${index})" title="Delete">×</button>
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

async function removeInstance(index) {
  const removedInstance = vrchatInstances[index];
  if (!removedInstance) return;

  if (removedInstance.status === 'running') {
    if (!confirm(`Profile ${removedInstance.profile} is running. Stop and delete?`)) {
      return;
    }
  }

  stopAuto(removedInstance);

  try {
    await tauriInvoke('remove_profile_settings', { profile: removedInstance.profile });
  } catch (error) {
    addLogEntry(`Failed to remove profile settings (Profile ${removedInstance.profile}): ${error}`);
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
      addLogEntry('EAC launcher is already running. Open aborted');
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
      addLogEntry(`${result.message} (PID: ${result.process_id})`);
    } else {
      addLogEntry(result.message);
    }
  } catch (error) {
    addLogEntry(`Failed to launch VRChat Profile ${instance.profile}: ${error}`);
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
      stopAuto(instance);
      renderList();
      addLogEntry(`${result.message} (PID: ${oldPid})`);
    } else {
      addLogEntry(result.message);
    }
  } catch (error) {
    addLogEntry(`Failed to stop VRChat Profile ${instance.profile}: ${error}`);
  }
}

async function openSetting(index) {
  const instance = vrchatInstances[index];
  if (!instance) return;

  try {
    await tauriInvoke('create_sub_window', { profile: instance.profile });
    addLogEntry(`Setting window opened (Profile ${instance.profile})`);
  } catch (error) {
    addLogEntry(`Failed to create Setting window: ${error}`);
  }
}

async function toggleAuto(index) {
  const instance = vrchatInstances[index];
  if (!instance) return;

  if (instance.autoActive) {
    stopAuto(instance);
  } else {
    await startAuto(instance);
  }
  renderList();
}

function stopAuto(instance) {
  if (instance.autoJumpTimerId !== null) {
    clearInterval(instance.autoJumpTimerId);
    instance.autoJumpTimerId = null;
  }
  instance.autoActive = false;
}

async function startAuto(instance) {
  try {
    const settings = await tauriInvoke('get_profile_settings', { profile: instance.profile });
    const uiState = (settings && settings.__ui) || {};

    instance.autoActive = true;

    if (uiState.autoJump) {
      const seconds = uiState.autoJumpSeconds || 3.0;
      const ms = Math.max(200, Math.round(seconds * 1000));
      const profile = instance.profile;

      tauriInvoke('send_osc_jump', { profile }).catch(e => console.error('[Auto] Jump failed:', e));
      instance.autoJumpTimerId = setInterval(() => {
        tauriInvoke('send_osc_jump', { profile }).catch(e => console.error('[Auto] Jump failed:', e));
      }, ms);
    }

    if (uiState.autoStart) {
      tauriInvoke('run_osc_auto_start', { profile: instance.profile }).catch(e => console.error('[Auto] Start failed:', e));
    }
  } catch (error) {
    console.error('[Auto] Failed to start automation:', error);
    instance.autoActive = false;
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
window.toggleAuto = toggleAuto;

async function checkRunningProcesses() {
  try {
    const runningProcesses = await tauriInvoke('get_running_vrchat');

    let statusChanged = false;
    vrchatInstances.forEach((instance) => {
      const currentPid = runningProcesses[instance.profile];
      const isRunning = currentPid !== undefined;

      if (instance.status === 'launching') {
        if (isRunning) {
          instance.status = 'running';
          instance.processId = currentPid;
          instance.waitingForMainProcess = false;
          statusChanged = true;
          addLogEntry(`VRChat Profile ${instance.profile} main process started (PID: ${currentPid})`);
        }
      } else if (instance.status === 'running') {
        if (!isRunning) {
          instance.status = 'stopped';
          instance.processId = null;
          instance.waitingForMainProcess = false;
          stopAuto(instance);
          statusChanged = true;
          addLogEntry(`VRChat Profile ${instance.profile} stopped`);
        } else if (instance.processId !== currentPid) {
          const oldPid = instance.processId;
          instance.processId = currentPid;
          statusChanged = true;
          addLogEntry(`VRChat Profile ${instance.profile} process changed: ${oldPid} -> ${currentPid}`);
        }
      } else if (instance.status === 'stopped') {
        if (isRunning) {
          instance.status = 'running';
          instance.processId = currentPid;
          instance.waitingForMainProcess = false;
          statusChanged = true;
          addLogEntry(`VRChat Profile ${instance.profile} detected (PID: ${currentPid})`);
        }
      }
    });

    const knownProfiles = new Set(vrchatInstances.map((i) => i.profile));
    for (const [profileStr, pid] of Object.entries(runningProcesses)) {
      const profile = Number.parseInt(profileStr, 10);
      if (!knownProfiles.has(profile)) {
        const newInstance = {
          profile,
          name: `VRChat Profile ${profile}`,
          status: 'running',
          processId: pid,
          waitingForMainProcess: false,
          isExistingProcess: false,
          autoActive: false,
          autoJumpTimerId: null
        };
        vrchatInstances.push(newInstance);
        statusChanged = true;
        addLogEntry(`Existing VRChat Profile ${profile} auto-added (PID: ${pid})`);
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
      addLogEntry(isEacActive ? 'EAC launcher detected. Open button disabled' : 'EAC launcher not found. Open button enabled');
      renderList();
    }
  } catch (error) {
    console.error('EAC check error:', error);
  }
}

window.addEventListener('DOMContentLoaded', () => {
  itemList = document.querySelector('#instance_list');
  addButton = document.querySelector('#add-button');
  itemCount = document.querySelector('#item-count');

  addButton.addEventListener('click', addVRChatProfile);

  vrchatInstances = [];
  renderList();
  updateItemCount();

  addLogEntry('VRChat Manager initialized');
  addLogEntry("Click 'Add' to create a new VRChat profile");
  addLogEntry('VRChat path: C:\\Program Files (x86)\\Steam\\steamapps\\common\\VRChat\\start_protected_game.exe');

  setInterval(checkRunningProcesses, 3000);
  setInterval(checkEacLauncher, 2000);
  setTimeout(checkEacLauncher, 500);
  setTimeout(checkRunningProcesses, 1000);
});
