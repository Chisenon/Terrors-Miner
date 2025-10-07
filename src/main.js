const { invoke } = window.__TAURI__.core;

let itemInput;
let itemList;
let itemCount;
let addButton;
let clearButton;
let items = [];

function updateItemCount() {
  itemCount.textContent = `Items: ${items.length}`;
}

function createListItem(text, index) {
  const li = document.createElement('li');
  li.className = 'list-item';
  li.innerHTML = `
    <span class="list-item-number">${index + 1}</span>
    <span class="list-item-content">${text}</span>
    <button class="delete-button" onclick="removeItem(${index})">×</button>
  `;
  return li;
}

function addItem() {
  const text = itemInput.value.trim();
  if (text === '') return;
  
  items.push(text);
  renderList();
  itemInput.value = '';
  updateItemCount();
}

function removeItem(index) {
  items.splice(index, 1);
  renderList();
  updateItemCount();
}

function clearAll() {
  items = [];
  renderList();
  updateItemCount();
}

function renderList() {
  itemList.innerHTML = '';
  items.forEach((item, index) => {
    const listItem = createListItem(item, index);
    itemList.appendChild(listItem);
  });
}

// Global function for onclick handlers
window.removeItem = removeItem;

window.addEventListener("DOMContentLoaded", () => {
  itemInput = document.querySelector("#item-input");
  itemList = document.querySelector("#item-list");
  itemCount = document.querySelector("#item-count");
  addButton = document.querySelector("#add-button");
  clearButton = document.querySelector("#clear-button");

  // Add event listeners
  addButton.addEventListener("click", addItem);
  clearButton.addEventListener("click", clearAll);
  
  // Add item on Enter key press
  itemInput.addEventListener("keypress", (e) => {
    if (e.key === "Enter") {
      addItem();
    }
  });

  // Initialize with some sample items
  items = ["Sample Item 1", "Sample Item 2", "Sample Item 3", "Sample Item 4", "Sample Item 5","Sample Item 6","Sample Item 7","Sample Item 8","Sample Item 9","Sample Item 10"];
  renderList();
  updateItemCount();
});
