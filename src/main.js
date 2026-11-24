import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, ask } from "@tauri-apps/plugin-dialog";
import { openPath } from "@tauri-apps/plugin-opener";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebview } from "@tauri-apps/api/webview";

const $ = (selector) => document.querySelector(selector);
const $$ = (selector) => Array.from(document.querySelectorAll(selector));

let appWindow = null;
try {
  appWindow = getCurrentWindow();
} catch (err) {
  console.warn("Tauri window API unavailable:", err);
}

const state = {
  step: 0,
  src: "",
  dstParent: "",
  dstName: "",
  dst: "",
  linkType: "junction",
  report: null,
  record: null,
  records: [],
  busy: false,
  scanning: false,
  scanCancelling: false,
  undoing: false,
  historyOpen: false,
  lastBytes: 0,
  lastTime: 0,
  speed: 0,
  lastLogStage: "",
  lastLogAt: 0
};

function esc(value) {
  return String(value ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;"
  }[c]));
}

function fmtBytes(value) {
  const n = Number(value);
  if (!Number.isFinite(n) || n <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let index = 0;
  let v = n;
  while (v >= 1024 && index < units.length - 1) {
    v /= 1024;
    index += 1;
  }
  const digits = v >= 100 || index === 0 ? 0 : 1;
  return `${v.toFixed(digits)} ${units[index]}`;
}

function fmtInt(value) {
  return Number(value || 0).toLocaleString("en-US");
}

function baseName(path) {
  const parts = String(path).split(/[\\/]+/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : "";
}

function joinPath(parent, name) {
  return `${parent.replace(/[\\/]+$/, "")}\\${name}`;
}

function driveOf(path) {
  const m = String(path).match(/^[a-zA-Z]:/);
  return m ? m[0].toUpperCase() : "—";
}

function linkLabel(type) {
  return type === "symlink" ? "Symlink /D" : "Junction /J";
}

function statusMeta(status) {
  if (status === "completed") return { text: "已完成", cls: "done" };
  if (status === "completed_with_warning") return { text: "有提示", cls: "warning" };
  if (status === "revoked") return { text: "已撤销", cls: "revoked" };
  return { text: status || "未知", cls: "revoked" };
}

function toast(message, ms = 2800) {
  const el = $("#toast");
  el.textContent = message;
  el.classList.add("show");
  clearTimeout(toast._timer);
  toast._timer = setTimeout(() => el.classList.remove("show"), ms);
}

function showStep(step) {
  state.step = step;
  state.historyOpen = false;
  $$(".panel").forEach((panel) => {
    panel.classList.toggle("active", panel.dataset.panel === String(step));
  });
  $$(".step").forEach((el, index) => {
    el.classList.toggle("active", index === step);
    el.classList.toggle("done", index < step);
    el.querySelector(".step-no").textContent = index < step
      ? "✓"
      : String(index + 1).padStart(2, "0");
  });
  $(".stage").scrollTop = 0;
}

function setChipsDisabled(disabled) {
  $("#chipJunction").disabled = disabled;
  $("#chipSymlink").disabled = disabled;
  $("#pickSrc").disabled = disabled;
  $("#pickDst").disabled = disabled;
  $("#backToSelect").disabled = disabled;
}

function updateSelectionUI() {
  const hasSrc = Boolean(state.src);
  const hasDst = Boolean(state.dst);
  const srcPath = $("#srcPath");
  const dstPath = $("#dstPath");

  srcPath.textContent = hasSrc ? state.src : "尚未选择源目录";
  srcPath.classList.toggle("empty", !hasSrc);
  dstPath.textContent = hasDst ? state.dst : "尚未选择目标目录";
  dstPath.classList.toggle("empty", !hasDst);

  $("#srcMeta").textContent = hasSrc
    ? `源目录 · ${driveOf(state.src)} 盘`
    : "源目录 · 点击浏览，或把文件夹直接拖进来";
  $("#dstMeta").textContent = hasDst
    ? `目标目录 · ${driveOf(state.dst)} 盘 · 名称可自定义`
    : "目标目录 · 先选源目录，再选择目标父目录";

  $("#toPreflight").disabled = state.busy || !hasSrc || !hasDst;
  $("#pickDst").disabled = state.busy || !hasSrc;
  $("#moveHint").textContent = hasSrc && hasDst
    ? "点击开始预检：扫描体积、逐文件占用检查与目标盘空间"
    : "请选择源目录和目标位置";

  if (!state.report) {
    $("#releaseSize").textContent = "—";
  }
}

async function updateDiskInfo(path, prefix) {
  const text = $(`#${prefix}DiskText`);
  const gauge = $(`#${prefix}Gauge`);
  const foot = $(`#${prefix}DiskFoot`);
  const name = $(`#${prefix}DiskName`);

  if (!path) {
    name.textContent = prefix === "src" ? "源盘" : "目标盘";
    text.textContent = "—";
    gauge.style.width = "0%";
    foot.textContent = prefix === "src" ? "选择源目录后显示" : "选择目标目录后显示";
    return;
  }

  name.textContent = `${driveOf(path)}: ${prefix === "src" ? "源盘" : "目标盘"}`;
  try {
    const info = await invoke("disk_info", { path });
    const used = Math.max(0, info.totalBytes - info.freeBytes);
    const percent = info.totalBytes > 0 ? (used / info.totalBytes) * 100 : 0;
    text.textContent = `${fmtBytes(used)} / ${fmtBytes(info.totalBytes)}`;
    gauge.style.width = `${Math.min(100, percent)}%`;
    foot.textContent = `剩余 ${fmtBytes(info.freeBytes)}`;
  } catch (err) {
    text.textContent = "—";
    foot.textContent = "无法读取磁盘信息";
  }
}

function recomputeTarget() {
  const name = state.dstName || (state.src ? baseName(state.src) : "");
  state.dst = state.dstParent && name
    ? joinPath(state.dstParent, name)
    : "";
}

let pendingTargetParent = "";

function validateFolderName(name) {
  if (!name) return "名称不能为空";
  if (/[<>:"/\\|?*]/.test(name)) {
    return "名称包含 Windows 不允许的字符：< > : \" / \\ | ? *";
  }
  if (/[. ]$/.test(name)) return "名称不能以点或空格结尾";
  const reserved = [
    "CON", "PRN", "AUX", "NUL",
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"
  ];
  if (reserved.includes(name.split(".")[0].toUpperCase())) {
    return "这是 Windows 保留名称";
  }
  return "";
}

function closeNameDialog() {
  $("#nameDialog").classList.remove("show");
  $("#targetNameError").hidden = true;
}

function confirmTargetName() {
  const input = $("#targetNameInput");
  const name = input.value.trim();
  const problem = validateFolderName(name);
  if (problem) {
    $("#targetNameError").textContent = problem;
    $("#targetNameError").hidden = false;
    return;
  }
  state.dstParent = pendingTargetParent;
  state.dstName = name;
  state.report = null;
  hidePreflightError();
  recomputeTarget();
  closeNameDialog();
  updateSelectionUI();
  updateDiskInfo(state.dstParent, "dst");
}

async function applySourcePath(selected) {
  const hit = await protectedDirOf(selected);
  if (hit) {
    state.src = "";
    state.report = null;
    recomputeTarget();
    updateSelectionUI();
    updateDiskInfo(state.src, "src");
    showSelectError(
      `「${selected}」属于系统保护目录，不允许迁移。\n原因：${hit.reason}\n保护范围：${hit.path}`
    );
    toast("该目录受系统保护，已取消选择", 4200);
    return false;
  }
  hideSelectError();
  state.src = selected;
  state.report = null;
  hidePreflightError();
  recomputeTarget();
  updateSelectionUI();
  updateDiskInfo(state.src, "src");
  if (state.dstParent) updateDiskInfo(state.dstParent, "dst");
  return true;
}

async function pickSource() {
  if (state.busy) return;
  try {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "选择源文件夹"
    });
    if (!selected || typeof selected !== "string") return;
    await applySourcePath(selected);
  } catch (err) {
    toast(`打开目录选择器失败：${String(err)}`);
  }
}

async function pickTarget() {
  if (state.busy) return;
  if (!state.src) {
    toast("请先选择源目录，再选择目标父目录");
    return;
  }
  try {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "选择目标父目录（下一步可自定义文件夹名称）"
    });
    if (!selected || typeof selected !== "string") return;
    if (!isDriveRoot(selected)) {
      const hit = await protectedDirOf(selected);
      if (hit) {
        showSelectError(
          `「${selected}」属于系统保护目录，不能作为迁移目标。\n原因：${hit.reason}\n保护范围：${hit.path}`
        );
        toast("该目录受系统保护，已取消选择", 4200);
        return;
      }
    }
    hideSelectError();

    pendingTargetParent = selected;
    $("#targetParentText").textContent = selected;
    $("#targetNameInput").value = state.dstName || baseName(state.src);
    $("#targetNameError").hidden = true;
    $("#nameDialog").classList.add("show");
    setTimeout(() => {
      const input = $("#targetNameInput");
      input.focus();
      input.select();
    }, 60);
  } catch (err) {
    toast(`打开目录选择器失败：${String(err)}`);
  }
}

function setDropHint(active, text) {
  const row = $("#srcRow");
  if (!row) return;
  row.classList.toggle("drop-active", active);
  if (active && text) $("#dropHintText").textContent = text;
}

async function handleDroppedPaths(paths) {
  setDropHint(false);
  const list = Array.isArray(paths) ? paths.filter(Boolean) : [];
  if (!list.length) return;
  if (state.busy) {
    toast("当前有任务正在进行，暂不能更换源目录", 3600);
    return;
  }

  const selected = list[0];
  let kind = "missing";
  try {
    kind = await invoke("path_kind", { path: selected });
  } catch (err) {
    console.warn("path kind check failed:", err);
  }
  if (kind === "file") {
    toast(`「${baseName(selected)}」是文件，请拖动文件夹`, 3600);
    return;
  }
  if (kind !== "dir") {
    toast("没有识别到这个文件夹，请重新拖动一次", 3600);
    return;
  }

  const name = baseName(selected);
  closeNameDialog();
  if (state.step !== 0) showStep(0);
  const applied = await applySourcePath(selected);
  if (!applied) return;
  toast(
    list.length > 1
      ? `已将「${name}」设为源目录（一次只取第一个文件夹）`
      : `已将「${name}」设为源目录`
  );
}

async function bindDropEvents() {
  try {
    const webview = getCurrentWebview();
    await webview.onDragDropEvent((event) => {
      const payload = event.payload || {};
      if (payload.type === "enter" || payload.type === "over") {
        if (state.busy) return;
        const first = Array.isArray(payload.paths) ? payload.paths[0] : "";
        setDropHint(
          true,
          first ? `松开鼠标，把「${baseName(first)}」设为源目录` : ""
        );
      } else if (payload.type === "leave") {
        setDropHint(false);
      } else if (payload.type === "drop") {
        handleDroppedPaths(payload.paths);
      }
    });
  } catch (err) {
    console.warn("Drag & drop unavailable:", err);
  }
}

function clearSelection() {
  state.src = "";
  state.dstParent = "";
  state.dstName = "";
  state.dst = "";
  state.report = null;
  state.record = null;
  state.lastBytes = 0;
  state.lastTime = 0;
  state.speed = 0;
  updateSelectionUI();
  updateDiskInfo("", "src");
  updateDiskInfo("", "dst");
  hidePreflightError();
  hideSelectError();
}

function hidePreflightError() {
  const box = $("#preflightError");
  box.hidden = true;
  box.textContent = "";
}

function showPreflightError(message) {
  const box = $("#preflightError");
  box.textContent = message;
  box.hidden = false;
}

function hideSelectError() {
  const box = $("#selectError");
  box.hidden = true;
  box.textContent = "";
}

function showSelectError(message) {
  const box = $("#selectError");
  box.textContent = message;
  box.hidden = false;
}

function isDriveRoot(path) {
  return /^[a-zA-Z]:\\?$/.test(String(path));
}

async function protectedDirOf(path) {
  try {
    return await invoke("check_protected_dir", { path });
  } catch (err) {
    // 旧版本后端没有该命令时放行，预检仍会再拦一次
    console.warn("protected dir check unavailable:", err);
    return null;
  }
}

function checkItem(kind, message, detail) {
  const icon = kind === "ok" ? "✓" : "!";
  const detailHtml = detail
    ? `<div class="check-detail">${esc(detail)}</div>`
    : "";
  return `<li class="${kind}"><span class="ck">${icon}</span><div>${esc(message)}${detailHtml}</div></li>`;
}

function renderLocked(report) {
  const panel = $("#lockedPanel");
  const list = $("#lockedList");
  const files = report.lockedFiles || [];

  if (!report.lockedCount) {
    panel.hidden = true;
    list.innerHTML = "";
    return;
  }

  $("#lockedHead").textContent =
    `占用 / 不可读文件 · 共 ${fmtInt(report.lockedCount)} 个${files.length < report.lockedCount ? `（列出前 ${files.length} 个）` : ""}`;
  list.innerHTML = files.map((file) => `
    <div class="locked-item">
      <span class="why">${esc(file.reason)}</span>
      <span>${esc(file.path)}</span>
    </div>
  `).join("");
  panel.hidden = false;
}

function renderPreflight(report) {
  $("#pfSize").textContent = fmtBytes(report.totalBytes);
  $("#pfFiles").textContent = fmtInt(report.fileCount);
  $("#pfDirs").textContent = fmtInt(report.dirCount);
  $("#pfFree").textContent = fmtBytes(report.targetFreeBytes);
  $("#pfSrc").textContent = report.source;
  $("#pfDst").textContent = report.target;
  $("#pfLinkType").textContent = report.linkType === "symlink" ? "SYMLINK /D" : "JUNCTION /J";
  $("#releaseSize").textContent = fmtBytes(report.totalBytes);
  $("#moveHint").innerHTML =
    `将移动 <span class="mono">${fmtBytes(report.totalBytes)}</span> 至 ${driveOf(report.target)} 目标盘`;

  const stamp = $("#preflightStamp");
  stamp.textContent = report.blocked ? "BLOCKED" : "PREFLIGHT PASSED";
  stamp.classList.toggle("blocked", report.blocked);

  const items = [];
  for (const issue of report.blockingIssues || []) {
    items.push(checkItem("error", issue.message, issue.detail));
  }
  for (const issue of report.warnings || []) {
    items.push(checkItem("warn", issue.message, issue.detail));
  }
  if (!items.length) {
    items.push(checkItem("ok", "所有文件均可独占读取，未发现被占用或不可读", null));
    items.push(checkItem("ok", `目标盘剩余 ${fmtBytes(report.targetFreeBytes)}，已预留 1 GB 安全余量`, null));
    items.push(checkItem("ok", "源目录中没有链接或重解析点，目标位置支持所选链接方式", null));
  }
  $("#pfChecks").innerHTML = items.join("");

  renderLocked(report);

  $("#startMigration").disabled = report.blocked;
  $("#preflightHint").textContent = report.blocked
    ? "存在阻断项，请处理后重新预检"
    : report.warnings?.length
      ? "预检通过，但有一条安全提示"
      : "预检通过，可以开始安全迁移";
  hidePreflightError();
}

function pushLog(text) {
  const log = $("#log");
  const now = new Date();
  const stamp = `${String(now.getHours()).padStart(2, "0")}:${String(now.getMinutes()).padStart(2, "0")}:${String(now.getSeconds()).padStart(2, "0")}`;
  const line = document.createElement("div");
  line.innerHTML = `<span class="ts">[${stamp}]</span> ${esc(text)}`;
  log.appendChild(line);
  while (log.childNodes.length > 200) log.removeChild(log.firstChild);
  log.scrollTop = log.scrollHeight;
}

function resetProgressUI() {
  state.lastBytes = 0;
  state.lastTime = 0;
  state.speed = 0;
  state.lastLogStage = "";
  state.lastLogAt = 0;
  $("#pct").textContent = "0";
  $("#barFill").style.width = "0%";
  $("#log").innerHTML = "";
  $("#liveSize").textContent = "0 B / —";
  $("#liveSpeed").textContent = "—";
  $("#liveEta").textContent = "—";
  $("#liveFiles").textContent = "0 / —";
  $("#currentFile").textContent = "准备迁移…";
  $("#phaseLabel").textContent = "准备迁移";
  $("#cancelBtn").disabled = false;
}

function etaText(seconds) {
  const total = Math.max(1, Math.round(seconds));
  if (total < 60) return `${total} 秒`;
  const minutes = Math.floor(total / 60);
  const rest = total % 60;
  return `${minutes} 分 ${rest} 秒`;
}

function updateProgress(payload) {
  const percent = Math.max(0, Math.min(100, Number(payload.percent || 0)));
  $("#pct").textContent = String(Math.floor(percent));
  $("#barFill").style.width = `${percent}%`;
  $("#liveSize").textContent = `${fmtBytes(payload.bytesDone)} / ${fmtBytes(payload.bytesTotal)}`;
  $("#liveFiles").textContent = `${fmtInt(payload.filesDone)} / ${fmtInt(payload.filesTotal)}`;
  $("#phaseLabel").textContent = payload.phaseLabel || "迁移中";
  if (payload.currentFile) {
    $("#currentFile").textContent = `正在处理 ${payload.currentFile}`;
  } else if (payload.message) {
    $("#currentFile").textContent = payload.message;
  }

  const now = performance.now();
  if (!state.lastTime) {
    state.lastTime = now;
    state.lastBytes = payload.bytesDone;
  } else if (now - state.lastTime > 400) {
    const deltaBytes = Math.max(0, payload.bytesDone - state.lastBytes);
    const deltaSeconds = (now - state.lastTime) / 1000;
    if (deltaSeconds > 0) state.speed = deltaBytes / deltaSeconds;
    state.lastTime = now;
    state.lastBytes = payload.bytesDone;
  }

  if (payload.stage === "copying" && state.speed > 0) {
    $("#liveSpeed").textContent = `${fmtBytes(state.speed)}/s`;
    const remaining = Math.max(0, payload.bytesTotal - payload.bytesDone);
    $("#liveEta").textContent = remaining > 0 ? `约 ${etaText(remaining / state.speed)}` : "即将完成";
  } else {
    $("#liveSpeed").textContent = "—";
    $("#liveEta").textContent = "—";
  }

  const nowMs = Date.now();
  if (
    payload.message ||
    payload.stage !== state.lastLogStage ||
    nowMs - state.lastLogAt > 650
  ) {
    state.lastLogStage = payload.stage;
    state.lastLogAt = nowMs;
    if (payload.message) {
      pushLog(payload.message);
    } else if (payload.currentFile) {
      pushLog(`复制 ${payload.currentFile}`);
    }
  }
}

async function runPreflight() {
  if (state.busy || !state.src || !state.dst) return;
  const button = $("#toPreflight");
  state.busy = true;
  state.scanning = true;
  state.scanCancelling = false;
  setChipsDisabled(true);
  button.disabled = false;
  button.classList.remove("primary");
  button.classList.add("danger");
  button.textContent = "取消扫描";
  $("#scanNote").textContent = "正在扫描源目录并检查文件占用…";
  hidePreflightError();

  try {
    const report = await invoke("preflight", {
      source: state.src,
      target: state.dst,
      linkType: state.linkType
    });
    const cancelled = (report.blockingIssues || []).some(
      (issue) => issue.code === "cancelled"
    );
    if (cancelled) {
      state.report = null;
      $("#scanNote").textContent = "";
      toast("已取消扫描，源目录未做任何改动");
      return;
    }
    state.report = report;
    renderPreflight(report);
    showStep(1);
  } catch (err) {
    toast(`预检失败：${String(err)}`);
    $("#scanNote").textContent = "";
  } finally {
    state.busy = false;
    state.scanning = false;
    state.scanCancelling = false;
    setChipsDisabled(false);
    button.classList.remove("danger");
    button.classList.add("primary");
    button.innerHTML = "开始预检 <span class=\"arrow\">→</span>";
    updateSelectionUI();
    updateDiskInfo(state.src, "src");
    if (state.dstParent) updateDiskInfo(state.dstParent, "dst");
  }
}

async function cancelScan() {
  const button = $("#toPreflight");
  state.scanCancelling = true;
  button.disabled = true;
  button.textContent = "正在取消…";
  $("#scanNote").textContent = "正在停止扫描…";
  try {
    await invoke("cancel_task");
  } catch (err) {
    state.scanCancelling = false;
    toast(String(err));
    button.disabled = false;
    button.textContent = "取消扫描";
  }
}

async function beginMigration() {
  const report = state.report;
  if (state.busy || !report || report.blocked) return;

  const confirmed = await ask(
    `即将迁移 ${fmtBytes(report.totalBytes)}（${fmtInt(report.fileCount)} 个文件）。\n\n源目录：${report.source}\n目标目录：${report.target}\n\n流程：复制到暂存区 → 校验 → 原目录改名备份 → 建立链接 → 验证后清理备份。任何一步失败都会自动回滚，原目录不会被删除。`,
    { title: "确认开始迁移", kind: "warning" }
  );
  if (!confirmed) return;

  state.busy = true;
  setChipsDisabled(true);
  resetProgressUI();
  showStep(2);

  try {
    const record = await invoke("start_migration", {
      source: report.source,
      target: report.target,
      linkType: report.linkType,
      expectedBytes: report.totalBytes,
      expectedFiles: report.fileCount,
      expectedDirs: report.dirCount
    });
    state.record = record;
    renderDone(record);
    await refreshRecords();
    showStep(3);
    toast(record.note
      ? "迁移已完成，但有提示，请查看完成页说明"
      : "迁移完成，原路径已由链接接管");
  } catch (err) {
    state.report = null;
    showStep(1);
    showPreflightError(String(err));
    $("#preflightHint").textContent = "迁移已中止，请返回修改并重新预检";
    toast(`迁移已安全中止：${String(err)}`, 4200);
    await refreshRecords();
  } finally {
    state.busy = false;
    setChipsDisabled(false);
    $("#startMigration").disabled = !state.report || state.report.blocked;
  }
}

function renderDone(record) {
  $("#doneSize").textContent = fmtBytes(record.sizeBytes);
  $("#doneSrc").textContent = record.source;
  $("#doneDst").textContent = record.target;
  $("#doneLinkType").textContent = record.linkType === "symlink" ? "SYMLINK /D" : "JUNCTION /J";
  $("#doneMeta").textContent =
    `${record.createdAt} · ${fmtInt(record.fileCount)} 个文件 · ${linkLabel(record.linkType)}`;

  const warning = $("#doneWarning");
  if (record.note) {
    warning.textContent = record.note;
    warning.hidden = false;
  } else {
    warning.textContent = "";
    warning.hidden = true;
  }
}

async function refreshRecords() {
  try {
    state.records = await invoke("list_records");
    $("#historyCount").textContent = state.records.length;
    if (state.historyOpen) renderRecords();
  } catch (err) {
    // 记录读取失败不应影响迁移主流程
    console.warn(err);
  }
}

function renderRecords() {
  const list = $("#recordList");
  const records = state.records || [];
  const released = records
    .filter((record) => record.status !== "revoked")
    .reduce((sum, record) => sum + Number(record.sizeBytes || 0), 0);

  $("#historySummary").textContent = records.length
    ? `本机共 ${records.length} 次迁移 · 累计释放 ${fmtBytes(released)}`
    : "本机暂无迁移记录";

  if (!records.length) {
    list.innerHTML = `<div class="history-empty">迁移完成后会自动写入一条记录，包含路径、体积、链接方式与状态。<br>记录只保存在本机。</div>`;
    return;
  }

  list.innerHTML = records.map((record) => {
    const meta = statusMeta(record.status);
    const canUndo = record.status !== "revoked";
    return `
      <article class="record${record.status === "revoked" ? " revoked" : ""}">
        <div class="record-head">
          <span class="status ${meta.cls}">${esc(meta.text)}</span>
          <span class="record-time">${esc(record.createdAt)}</span>
          <span class="record-link">${esc(linkLabel(record.linkType))}</span>
        </div>
        <div class="record-paths">
          <div>${esc(record.source)}</div>
          <div class="to"><span class="arrow">↓</span> ${esc(record.target)}</div>
        </div>
        <div class="record-meta">${fmtBytes(record.sizeBytes)} · ${fmtInt(record.fileCount)} 个文件</div>
        ${record.note ? `<div class="record-note">${esc(record.note)}</div>` : ""}
        <div class="record-actions">
          ${record.status !== "revoked" ? `<button class="btn ghost small" data-act="open" data-id="${esc(record.id)}">打开目标</button>` : ""}
          ${canUndo ? `<button class="btn ghost small danger-text" data-act="undo" data-id="${esc(record.id)}">撤销</button>` : ""}
        </div>
      </article>
    `;
  }).join("");
}

async function showHistory() {
  state.historyOpen = true;
  state.undoing = false;
  $$(".panel").forEach((panel) => {
    panel.classList.toggle("active", panel.dataset.panel === "history");
  });
  $$(".step").forEach((el) => el.classList.remove("active"));
  $("#historyHint").textContent = "";
  $(".stage").scrollTop = 0;
  await refreshRecords();
  renderRecords();
}

async function undoRecord(record, after) {
  if (state.busy || !record) return;
  const confirmed = await ask(
    `撤销会先把目标盘的数据复制回原路径，校验通过后才移除链接。\n\n源路径：${record.source}\n目标路径：${record.target}\n\n确定撤销这次迁移吗？`,
    { title: "确认撤销迁移", kind: "warning" }
  );
  if (!confirmed) return;

  state.busy = true;
  state.undoing = true;
  $("#historyHint").textContent = "正在准备撤销…";

  try {
    await invoke("undo_migration", { id: record.id });
    toast("撤销完成，文件已恢复原路径", 3400);
    await refreshRecords();
    renderRecords();
    if (after === "done") {
      clearSelection();
      showStep(0);
    }
  } catch (err) {
    toast(`撤销失败：${String(err)}`, 4200);
  } finally {
    state.busy = false;
    state.undoing = false;
    $("#historyHint").textContent = "";
  }
}

async function loadElevation() {
  try {
    const elevated = await invoke("is_elevated");
    const badge = $("#adminBadge");
    badge.innerHTML = `<span class="dot"></span>${elevated ? "已提权 · 管理员" : "未提权 · 受限"}`;
    badge.classList.toggle("no-admin", !elevated);
    $("#adminBanner").classList.toggle("show", !elevated);
  } catch {
    $("#adminBadge").innerHTML = '<span class="dot"></span>权限未知';
  }
}

async function bindTauriEvents() {
  await listen("moveonlink://scan", (event) => {
    if (state.step !== 0 || !state.busy || state.scanCancelling) return;
    const payload = event.payload;
    $("#scanNote").textContent =
      `已扫描 ${fmtInt(payload.files)} 个文件 · ${fmtBytes(payload.bytes)}`;
  });

  await listen("moveonlink://progress", (event) => {
    const payload = event.payload;
    if (state.undoing) {
      $("#historyHint").textContent =
        `${payload.phaseLabel || "正在撤销"} · ${Math.floor(payload.percent || 0)}%`;
      return;
    }
    if (state.historyOpen) {
      $("#historyHint").textContent =
        `${payload.phaseLabel || "迁移中"} · ${Math.floor(payload.percent || 0)}%`;
      return;
    }
    updateProgress(payload);
  });
}

function bindUI() {
  $("#btnMin").addEventListener("click", () => {
    appWindow?.minimize().catch((err) => console.warn(err));
  });
  $("#btnMax").addEventListener("click", () => {
    appWindow?.toggleMaximize().catch((err) => console.warn(err));
  });
  $("#btnClose").addEventListener("click", async () => {
    if (state.busy) {
      const confirmed = await ask(
        "迁移正在进行。此时强制关闭可能中断本次操作（源目录有备份保护，但建议先取消迁移）。确定关闭吗？",
        { title: "迁移进行中", kind: "warning" }
      );
      if (!confirmed) return;
    }
    appWindow?.close().catch((err) => console.warn(err));
  });

  $("#pickSrc").addEventListener("click", pickSource);
  $("#pickDst").addEventListener("click", pickTarget);

  $("#targetNameClose").addEventListener("click", closeNameDialog);
  $("#targetNameCancel").addEventListener("click", closeNameDialog);
  $("#targetNameConfirm").addEventListener("click", confirmTargetName);
  $("#targetNameInput").addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      confirmTargetName();
    }
  });
  $("#targetNameInput").addEventListener("input", () => {
    $("#targetNameError").hidden = true;
  });
  $("#nameDialog").addEventListener("click", (event) => {
    if (event.target === $("#nameDialog")) closeNameDialog();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") closeNameDialog();
  });

  $$(".chip").forEach((chip) => {
    chip.addEventListener("click", () => {
      if (state.busy) return;
      $$(".chip").forEach((other) => other.classList.remove("on"));
      chip.classList.add("on");
      state.linkType = chip.dataset.link === "symlink" ? "symlink" : "junction";
      state.report = null;
      $("#railNote").innerHTML = state.linkType === "symlink"
        ? '符号链接 <span class="mono">mklink /D</span> · 需管理员或开发者模式'
        : '默认 <span class="mono">junction /J</span> · 无需开发者模式';
      updateSelectionUI();
    });
  });

  $("#toPreflight").addEventListener("click", () => {
    if (state.scanning) cancelScan();
    else runPreflight();
  });
  $("#backToSelect").addEventListener("click", () => showStep(0));
  $("#startMigration").addEventListener("click", beginMigration);
  $("#cancelBtn").addEventListener("click", async () => {
    if (!state.busy) return;
    $("#cancelBtn").disabled = true;
    pushLog("收到取消请求，正在安全清理暂存副本…");
    try {
      await invoke("cancel_task");
    } catch (err) {
      toast(String(err));
    }
  });

  $("#historyBtn").addEventListener("click", () => {
    if (state.historyOpen) showStep(state.step);
    else showHistory();
  });
  $("#backToWizard").addEventListener("click", () => showStep(state.step));

  $("#recordList").addEventListener("click", async (event) => {
    const button = event.target.closest("button[data-act]");
    if (!button) return;
    const record = state.records.find((item) => item.id === button.dataset.id);
    if (!record) return;

    if (button.dataset.act === "open") {
      openPath(record.target).catch((err) => toast(`打开目录失败：${String(err)}`));
      return;
    }

    if (button.dataset.act === "undo") {
      if (state.busy) return;
      await undoRecord(record, state.historyOpen ? "history" : "done");
    }
  });

  $("#openTarget").addEventListener("click", () => {
    if (!state.record) return;
    openPath(state.record.target).catch((err) => toast(`打开目录失败：${String(err)}`));
  });

  $("#undoBtn").addEventListener("click", () => {
    if (!state.record) return;
    undoRecord(state.record, "done");
  });

  $("#restart").addEventListener("click", () => {
    if (state.busy) return;
    clearSelection();
    showStep(0);
  });
}

(async function init() {
  bindUI();
  try {
    await bindTauriEvents();
    await bindDropEvents();
    await loadElevation();
  } catch (err) {
    console.warn("Tauri bridge unavailable:", err);
  }
  await refreshRecords();
  try {
    const notice = await invoke("take_recovery_notice");
    if (notice) toast(notice, 8000);
  } catch {
    // 浏览器预览或旧版本后端时忽略
  }
  updateSelectionUI();
  showStep(0);
})();
