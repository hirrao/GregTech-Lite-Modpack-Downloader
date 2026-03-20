import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./styles.css";

const app = document.querySelector("#app");

app.innerHTML = `
  <main class="layout">
    <section class="card">
      <h1>GregTech Lite 下载器</h1>
      <form id="install-form" class="form">
        <label>
          下载目录
          <input id="output-dir" type="text" />
          <small>留空使用默认下载目录。</small>
        </label>

        <label>
          文件名
          <input id="output-name" type="text" value="gregtech-lite-nightly-curseforge.zip" />
        </label>

        <label>
          下载代理
          <select id="proxy-mode">
            <option value="none">不使用</option>
            <option value="preset">使用预置</option>
            <option value="custom">使用自定义</option>
          </select>
          <small>预置代理：gh-proxy</small>
        </label>

        <label id="custom-proxy-field" class="field hidden">
          自定义代理
          <input id="custom-proxy" type="text" placeholder="例如 https://gh-proxy.org/" />
        </label>

        <button id="run-btn" type="submit">开始下载</button>
      </form>
    </section>

    <section class="card">
      <div class="status-row">
        <h2>状态</h2>
        <span id="status" class="status idle">待命</span>
      </div>

      <div class="progress-panel">
        <div class="progress-head">
          <span id="progress-label">待命</span>
          <span id="progress-text" class="progress-text">0%</span>
        </div>
        <div class="progress-track">
          <div id="progress-bar" class="progress-bar"></div>
        </div>
        <p id="download-meta" class="download-meta">已下载 0 B</p>
      </div>

      <pre id="logs" class="logs">暂无日志</pre>
      <p id="result" class="result"></p>
    </section>
  </main>
`;

const ui = {
  form: document.querySelector("#install-form"),
  outputDir: document.querySelector("#output-dir"),
  outputName: document.querySelector("#output-name"),
  proxyMode: document.querySelector("#proxy-mode"),
  customProxyField: document.querySelector("#custom-proxy-field"),
  customProxy: document.querySelector("#custom-proxy"),
  logs: document.querySelector("#logs"),
  result: document.querySelector("#result"),
  status: document.querySelector("#status"),
  runButton: document.querySelector("#run-btn"),
  progressLabel: document.querySelector("#progress-label"),
  progressText: document.querySelector("#progress-text"),
  progressBar: document.querySelector("#progress-bar"),
  downloadMeta: document.querySelector("#download-meta"),
};

const state = {
  logs: [],
};

function setStatus(kind, text) {
  ui.status.className = `status ${kind}`;
  ui.status.textContent = text;
}

function renderLogs() {
  ui.logs.textContent = state.logs.length > 0 ? state.logs.join("\n") : "暂无日志";
  ui.logs.scrollTop = ui.logs.scrollHeight;
}

function clearLogs() {
  state.logs = [];
  renderLogs();
}

function appendLog(line) {
  if (!line) {
    return;
  }

  state.logs.push(line);
  renderLogs();
}

function appendLogOnce(line) {
  if (!line || state.logs.at(-1) === line) {
    return;
  }

  appendLog(line);
}

function formatBytes(bytes) {
  if (bytes == null || Number.isNaN(bytes)) {
    return "未知";
  }

  const units = ["B", "KB", "MB", "GB"];
  let value = Number(bytes);
  let unitIndex = 0;

  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }

  return unitIndex === 0
    ? `${Math.round(value)} ${units[unitIndex]}`
    : `${value.toFixed(1)} ${units[unitIndex]}`;
}

function setProgress(downloadedBytes = 0, totalBytes = null) {
  const downloaded = Number(downloadedBytes) || 0;
  const total = Number.isFinite(Number(totalBytes)) ? Number(totalBytes) : null;
  const percent = total && total > 0 ? Math.min((downloaded / total) * 100, 100) : 0;

  ui.progressBar.style.width = `${percent}%`;
  ui.progressText.textContent = total ? `${percent.toFixed(1)}%` : "进行中";
  ui.downloadMeta.textContent = total
    ? `已下载 ${formatBytes(downloaded)} / ${formatBytes(total)}`
    : `已下载 ${formatBytes(downloaded)}`;
}

function setProgressLabel(text) {
  ui.progressLabel.textContent = text;
}

function setResult(path = "") {
  ui.result.textContent = path ? `保存位置：${path}` : "";
}

function setBusy(isBusy) {
  ui.runButton.disabled = isBusy;
}

function updateProxyFieldVisibility() {
  const isCustom = ui.proxyMode.value === "custom";
  ui.customProxyField.classList.toggle("hidden", !isCustom);
  ui.customProxy.disabled = !isCustom;

  if (!isCustom) {
    ui.customProxy.value = "";
  }
}

function resetViewForRun() {
  setStatus("running", "下载中");
  setProgressLabel("准备中");
  setProgress(0, null);
  setResult("");
  clearLogs();
  setBusy(true);
}

function applyDownloadEvent(payload = {}) {
  if (payload.message) {
    appendLog(payload.message);
    setProgressLabel(payload.message);
  }

  if (payload.stage === "running") {
    setStatus("running", "下载中");
    setProgressLabel("下载中");
    setProgress(payload.downloaded_bytes, payload.total_bytes);
  }

  if (payload.stage === "completed") {
    setStatus("success", "完成");
    setProgressLabel("已完成");
    setProgress(payload.downloaded_bytes, payload.total_bytes ?? payload.downloaded_bytes);
  }

  if (payload.stage === "error") {
    setStatus("error", "失败");
    setProgressLabel("失败");
  }

  if (payload.output_path) {
    setResult(payload.output_path);
  }
}

function validateForm() {
  const values = {
    outputFilename: ui.outputName.value.trim(),
    proxyMode: ui.proxyMode.value,
    customProxy: ui.customProxy.value.trim(),
    outputDir: ui.outputDir.value.trim() || null,
  };

  if (!values.outputFilename) {
    return { error: "文件名不能为空。" };
  }

  if (values.proxyMode === "custom" && !values.customProxy) {
    return { error: "自定义代理不能为空。" };
  }

  return { values };
}

listen("download-progress", (event) => {
  applyDownloadEvent(event.payload || {});
}).catch((error) => {
  ui.logs.textContent = `事件监听失败：${String(error)}`;
});

ui.proxyMode.addEventListener("change", updateProxyFieldVisibility);
updateProxyFieldVisibility();

ui.form.addEventListener("submit", async (event) => {
  event.preventDefault();

  const { values, error } = validateForm();
  if (error) {
    setStatus("error", "参数错误");
    ui.logs.textContent = error;
    return;
  }

  resetViewForRun();

  try {
    const res = await invoke("run_install", {
      outputDir: values.outputDir,
      outputFilename: values.outputFilename,
      proxyMode: values.proxyMode,
      customProxy: values.proxyMode === "custom" ? values.customProxy : null,
    });

    setResult(res.output_path);
  } catch (invokeError) {
    const message = String(invokeError);
    setStatus("error", "失败");
    setProgressLabel("失败");
    appendLogOnce(message);
  } finally {
    setBusy(false);
  }
});
