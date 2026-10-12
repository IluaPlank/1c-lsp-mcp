const DEFAULT_BSL = `{
  "$schema": "https://1c-syntax.github.io/bsl-language-server/configuration/schema.json",
  "language": "ru",
  "diagnostics": {
    "minimumLSPDiagnosticLevel": "Error",
    "parameters": {
      "MethodSize": false
    }
  }
}`;

const state = {
  projects: [],
  selectedId: null,
  isNew: false,
  pollTimer: null,
  settings: null,
};

function $(id) {
  return document.getElementById(id);
}

function statusLabel(status) {
  switch (status) {
    case "ready":
      return "Готов";
    case "starting":
      return "Запускается";
    case "warming_up":
      return "Индексация";
    case "error":
      return "Ошибка";
    case "stopped":
      return "Остановлен";
    default:
      return status || "Неизвестно";
  }
}

function statusClass(status) {
  return status || "stopped";
}

async function api(path, options = {}) {
  const response = await fetch(path, {
    headers: { "Content-Type": "application/json", ...(options.headers || {}) },
    ...options,
  });
  if (response.status === 204) return null;
  const text = await response.text();
  let data = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = text;
    }
  }
  if (!response.ok) {
    const msg =
      (data && (data.error || data.message || data.detail)) ||
      (typeof data === "string" ? data : null) ||
      `HTTP ${response.status}`;
    throw new Error(msg);
  }
  return data;
}

function showToast(message, isError = false) {
  const el = $("toast");
  el.textContent = message;
  el.classList.toggle("hidden", false);
  el.style.background = isError ? "#450a0a" : "#052e16";
  el.style.borderColor = isError ? "#7f1d1d" : "#166534";
  el.style.color = isError ? "#fecaca" : "#bbf7d0";
  clearTimeout(showToast._t);
  showToast._t = setTimeout(() => el.classList.add("hidden"), 3500);
}

function selectedProject() {
  if (!state.selectedId) return null;
  return state.projects.find((p) => p.id === state.selectedId) || null;
}

function renderList() {
  const list = $("project-list");
  if (!state.projects.length && !state.isNew) {
    list.textContent = "Проектов пока нет.";
    return;
  }
  list.innerHTML = "";
  for (const project of state.projects) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className =
      "project-item" + (project.id === state.selectedId && !state.isNew ? " active" : "");
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = project.name;
    const badge = document.createElement("span");
    badge.className = `badge ${statusClass(project.status.status)}`;
    badge.textContent = statusLabel(project.status.status);
    btn.append(name, badge);
    btn.onclick = () => selectProject(project.id);
    list.appendChild(btn);
  }
}

function fillForm(project) {
  $("empty-state").classList.add("hidden");
  $("project-form").classList.remove("hidden");

  if (!project) {
    $("form-title").textContent = "Новый проект";
    $("f-id").textContent = "(будет присвоен после сохранения)";
    $("f-name").value = "";
    $("f-project-root").value = "";
    $("f-root").value = "";
    $("f-jvm").value = "";
    $("f-debug").checked = false;
    $("f-bsl").value = DEFAULT_BSL;
    $("status-row").innerHTML = "";
    $("progress-wrap").classList.add("hidden");
    $("error-banner").classList.add("hidden");
    $("logs").textContent = "—";
    $("btn-start").disabled = true;
    $("btn-stop").disabled = true;
    $("wait-hint").classList.add("hidden");
    $("btn-delete").disabled = true;
    $("btn-copy-id").disabled = true;
    return;
  }

  $("form-title").textContent = project.name;
  $("f-id").textContent = project.id;
  $("f-name").value = project.name || "";
  $("f-project-root").value = project.project_root_path || "";
  $("f-root").value = project.root_path || "";
  $("f-jvm").value = project.jvm_args || "";
  $("f-debug").checked = !!project.debug;
  $("f-bsl").value = project.bsl_config || DEFAULT_BSL;

  const st = project.status?.status || "stopped";
  $("status-row").innerHTML = `<span class="badge ${statusClass(st)}">${statusLabel(st)}</span>`;

  const err = project.status?.error;
  if (err) {
    $("error-banner").textContent = err;
    $("error-banner").classList.remove("hidden");
  } else {
    $("error-banner").classList.add("hidden");
  }

  const prog = project.progress || {};
  if (prog.active || prog.percentage != null) {
    $("progress-wrap").classList.remove("hidden");
    const pct = prog.percentage != null ? prog.percentage : 0;
    $("progress-fill").style.width = `${pct}%`;
    const parts = [];
    if (prog.message) parts.push(prog.message);
    if (prog.files_done != null && prog.files_total != null) {
      parts.push(`${prog.files_done} / ${prog.files_total} файлов`);
    }
    if (prog.percentage != null) parts.push(`${prog.percentage}%`);
    $("progress-text").textContent = parts.join(" · ") || "Индексация…";
  } else {
    $("progress-wrap").classList.add("hidden");
  }

  const running = st === "ready" || st === "starting" || st === "warming_up";
  $("btn-start").disabled = running;
  $("btn-stop").disabled = st === "stopped";
  $("wait-hint").classList.toggle("hidden", st !== "starting");
  $("btn-delete").disabled = false;
  $("btn-copy-id").disabled = false;
}

function selectProject(id) {
  state.isNew = false;
  state.selectedId = id;
  renderList();
  fillForm(selectedProject());
  refreshLogs().catch(() => {});
}

function startNew() {
  state.isNew = true;
  state.selectedId = null;
  renderList();
  fillForm(null);
}

function payloadFromForm() {
  return {
    name: $("f-name").value.trim(),
    project_root_path: $("f-project-root").value.trim(),
    root_path: $("f-root").value.trim(),
    jvm_args: $("f-jvm").value.trim(),
    bsl_config: $("f-bsl").value.trim(),
    debug: $("f-debug").checked,
  };
}

async function loadProjects({ keepSelection = true } = {}) {
  state.projects = await api("/api/projects");
  if (keepSelection && state.selectedId) {
    if (!state.projects.some((p) => p.id === state.selectedId)) {
      state.selectedId = null;
      state.isNew = false;
      $("project-form").classList.add("hidden");
      $("empty-state").classList.remove("hidden");
    }
  }
  renderList();
  if (state.isNew) {
    fillForm(null);
  } else if (state.selectedId) {
    fillForm(selectedProject());
  }
}

async function saveProject(event) {
  event.preventDefault();
  const payload = payloadFromForm();
  try {
    let project;
    if (state.isNew || !state.selectedId) {
      project = await api("/api/projects", {
        method: "POST",
        body: JSON.stringify(payload),
      });
      state.isNew = false;
      state.selectedId = project.id;
      showToast("Проект создан");
    } else {
      project = await api(`/api/projects/${state.selectedId}`, {
        method: "PUT",
        body: JSON.stringify(payload),
      });
      showToast("Сохранено");
    }
    await loadProjects();
    fillForm(selectedProject());
  } catch (err) {
    showToast(err.message, true);
  }
}

async function startProject() {
  if (!state.selectedId) return;
  try {
    $("btn-start").disabled = true;
    await api(`/api/projects/${state.selectedId}/start`, { method: "POST" });
    showToast("BSL ответил, идёт индексация");
    await loadProjects();
  } catch (err) {
    const cancelled = /остановлен пользователем/i.test(err.message);
    showToast(cancelled ? "Запуск остановлен" : err.message, !cancelled);
    await loadProjects();
  }
}

async function stopProject() {
  if (!state.selectedId) return;
  try {
    $("btn-stop").disabled = true;
    await api(`/api/projects/${state.selectedId}/stop`, { method: "POST" });
    showToast("Остановлен");
    await loadProjects();
  } catch (err) {
    showToast(err.message, true);
    await loadProjects();
  }
}

async function deleteProject() {
  if (!state.selectedId) return;
  const name = selectedProject()?.name || state.selectedId;
  if (!confirm(`Удалить проект «${name}»?`)) return;
  try {
    await api(`/api/projects/${state.selectedId}`, { method: "DELETE" });
    state.selectedId = null;
    state.isNew = false;
    $("project-form").classList.add("hidden");
    $("empty-state").classList.remove("hidden");
    showToast("Удалён");
    await loadProjects({ keepSelection: false });
  } catch (err) {
    showToast(err.message, true);
  }
}

async function refreshLogs() {
  if (!state.selectedId || state.isNew) {
    $("logs").textContent = "—";
    return;
  }
  const lines = await api(`/api/projects/${state.selectedId}/logs?tail=200`);
  $("logs").textContent = (lines && lines.length ? lines.join("\n") : "(пусто)");
}

async function clearLogs() {
  if (!state.selectedId) return;
  await api(`/api/projects/${state.selectedId}/logs`, { method: "DELETE" });
  $("logs").textContent = "(пусто)";
  showToast("Логи очищены");
}

function copyId() {
  const id = $("f-id").textContent;
  if (!id || id.startsWith("(")) return;
  navigator.clipboard.writeText(id).then(
    () => showToast("ID скопирован"),
    () => showToast("Не удалось скопировать", true)
  );
}

function connectEvents() {
  try {
    const es = new EventSource("/api/events");
    es.onmessage = () => {
      loadProjects().catch(() => {});
    };
    es.addEventListener("ProjectStatusChanged", () => loadProjects().catch(() => {}));
    es.addEventListener("IndexingProgress", () => loadProjects().catch(() => {}));
    es.onerror = () => {
      /* fallback to poll */
    };
  } catch {
    /* ignore */
  }
  state.pollTimer = setInterval(() => {
    loadProjects().catch(() => {});
  }, 5000);
}

function fillSettingsForm() {
  const settings = state.settings;
  if (!settings) return;
  $("s-toon").checked = !!settings.use_toon_format;
  const level = settings.log_level || "info";
  const select = $("s-log-level");
  if (![...select.options].some((option) => option.value === level)) {
    const extra = document.createElement("option");
    extra.value = level;
    extra.textContent = level;
    select.appendChild(extra);
  }
  select.value = level;
}

function openSettings() {
  fillSettingsForm();
  $("settings-note").classList.add("hidden");
  $("settings-dialog").classList.remove("hidden");
}

function closeSettings() {
  $("settings-dialog").classList.add("hidden");
}

async function loadSettings() {
  state.settings = await api("/api/settings");
  const hint = $("server-hint");
  if (state.settings && hint) {
    hint.textContent = `MCP: ${state.settings.mcp_port} · UI: ${state.settings.http_port}`;
  }
  if (!$("settings-dialog").classList.contains("hidden")) {
    fillSettingsForm();
  }
}

async function saveSettings(event) {
  event.preventDefault();
  const current = state.settings;
  if (!current) return;
  const previousLevel = current.log_level;
  const payload = {
    jar_path: current.jar_path,
    listen_host: current.listen_host,
    http_port: current.http_port,
    log_level: $("s-log-level").value,
    mcp_enabled: current.mcp_enabled,
    mcp_port: current.mcp_port,
    use_toon_format: $("s-toon").checked,
    platform_path: current.platform_path || "",
    bsl_context_jar_path: current.bsl_context_jar_path || "",
  };
  try {
    const saved = await api("/api/settings", {
      method: "PUT",
      body: JSON.stringify(payload),
    });
    state.settings = saved;
    fillSettingsForm();
    const notes = ["Настройки сохранены."];
    if (saved.restart_required) {
      notes.push("Нужен перезапуск сервера.");
    } else if (payload.log_level !== previousLevel) {
      notes.push("Уровень лога применится после перезапуска сервера.");
    }
    const message = notes.join(" ");
    $("settings-note").textContent = message;
    $("settings-note").classList.remove("hidden");
    showToast(message);
  } catch (err) {
    $("settings-note").textContent = err.message;
    $("settings-note").classList.remove("hidden");
    showToast(err.message, true);
  }
}

function bind() {
  $("btn-settings").onclick = () =>
    loadSettings()
      .then(openSettings)
      .catch((e) => showToast(e.message, true));
  $("btn-settings-close").onclick = closeSettings;
  $("settings-dialog").onclick = (event) => {
    if (event.target === $("settings-dialog")) closeSettings();
  };
  $("settings-form").onsubmit = saveSettings;
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") closeSettings();
  });
  $("btn-reload").onclick = () =>
    loadProjects()
      .then(() => showToast("Обновлено"))
      .catch((e) => showToast(e.message, true));
  $("btn-new").onclick = startNew;
  $("project-form").onsubmit = saveProject;
  $("btn-start").onclick = startProject;
  $("btn-stop").onclick = stopProject;
  $("btn-delete").onclick = deleteProject;
  $("btn-copy-id").onclick = copyId;
  $("btn-refresh-logs").onclick = () =>
    refreshLogs().catch((e) => showToast(e.message, true));
  $("btn-clear-logs").onclick = () =>
    clearLogs().catch((e) => showToast(e.message, true));
}

bind();
loadProjects()
  .then(() => {
    connectEvents();
    loadSettings().catch(() => {});
    if (state.projects.length === 1) {
      selectProject(state.projects[0].id);
    }
  })
  .catch((err) => {
    $("project-list").textContent = err.message;
  });
