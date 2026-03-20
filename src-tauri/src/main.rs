#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use reqwest::{
    blocking::{Client, Response},
    header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE},
    redirect::Policy,
    StatusCode,
};
use serde::Serialize;
use tauri::{command, AppHandle, Emitter};

const DOWNLOAD_EVENT: &str = "download-progress";
const MODPACK_URL: &str = "https://github.com/GregTechLite/GregTech-Lite-Modpack/releases/download/nightly/gregtech-lite-nightly-curseforge.zip";
const PRESET_PROXY_PREFIX: &str = "https://gh-proxy.org/";
const DEFAULT_FILENAME: &str = "gregtech-lite-nightly-curseforge.zip";
const BUFFER_SIZE: usize = 64 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);
const DEFAULT_PROXY_MODE: &str = "none";

type AppResult<T> = Result<T, AppError>;

#[derive(Debug)]
struct AppError {
    code: &'static str,
    message: String,
}

impl AppError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn with_source(
        code: &'static str,
        message: impl Into<String>,
        source: impl fmt::Display,
    ) -> Self {
        Self::new(code, format!("{}：{}", message.into(), source))
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

#[derive(Clone, Copy)]
enum ProxyMode {
    None,
    Preset,
    Custom,
}

impl ProxyMode {
    fn parse(value: Option<&str>) -> AppResult<Self> {
        match value.unwrap_or(DEFAULT_PROXY_MODE).trim() {
            "" | "none" => Ok(Self::None),
            "preset" => Ok(Self::Preset),
            "custom" => Ok(Self::Custom),
            other => Err(AppError::new("CFG-001", format!("未知代理模式：{other}"))),
        }
    }
}

struct DownloadConfig {
    output_dir: PathBuf,
    output_filename: String,
    proxy_mode: ProxyMode,
    custom_proxy: String,
}

impl DownloadConfig {
    fn from_command(
        output_dir: Option<String>,
        output_filename: Option<String>,
        proxy_mode: Option<String>,
        custom_proxy: Option<String>,
    ) -> AppResult<Self> {
        let output_filename = output_filename
            .unwrap_or_else(|| DEFAULT_FILENAME.to_string())
            .trim()
            .to_string();
        if output_filename.is_empty() {
            return Err(AppError::new("CFG-002", "文件名不能为空"));
        }

        Ok(Self {
            output_dir: output_dir
                .map(PathBuf::from)
                .unwrap_or_else(default_output_dir),
            output_filename,
            proxy_mode: ProxyMode::parse(proxy_mode.as_deref())?,
            custom_proxy: custom_proxy.unwrap_or_default(),
        })
    }

    fn output_path(&self) -> PathBuf {
        self.output_dir.join(&self.output_filename)
    }

    fn download_url(&self) -> AppResult<String> {
        match self.proxy_mode {
            ProxyMode::None => Ok(MODPACK_URL.to_string()),
            ProxyMode::Preset => Ok(apply_proxy_prefix(PRESET_PROXY_PREFIX, MODPACK_URL)),
            ProxyMode::Custom => {
                let prefix = self.custom_proxy.trim();
                if prefix.is_empty() {
                    return Err(AppError::new("CFG-003", "自定义代理地址不能为空"));
                }
                Ok(apply_proxy_prefix(prefix, MODPACK_URL))
            }
        }
    }

    fn ensure_output_dir(&self) -> AppResult<()> {
        fs::create_dir_all(&self.output_dir).map_err(|error| {
            AppError::with_source(
                "FS-001",
                format!("无法创建下载目录 {}", self.output_dir.display()),
                error,
            )
        })
    }
}

#[derive(Serialize)]
struct InstallResult {
    output_path: String,
}

#[derive(Clone, Serialize)]
struct DownloadEvent {
    stage: &'static str,
    message: Option<String>,
    downloaded_bytes: Option<u64>,
    total_bytes: Option<u64>,
    output_path: Option<String>,
}

struct Reporter<'a> {
    app: &'a AppHandle,
}

impl<'a> Reporter<'a> {
    fn new(app: &'a AppHandle) -> Self {
        Self { app }
    }

    fn info(&self, message: impl Into<String>) {
        self.emit("preparing", Some(message.into()), None, None, None);
    }

    fn progress(&self, downloaded_bytes: u64, total_bytes: Option<u64>) {
        self.emit("running", None, Some(downloaded_bytes), total_bytes, None);
    }

    fn done(&self, message: impl Into<String>, output_path: &Path, bytes: u64, total: Option<u64>) {
        self.emit(
            "completed",
            Some(message.into()),
            Some(bytes),
            total.or(Some(bytes)),
            Some(output_path.display().to_string()),
        );
    }

    fn fail(&self, error: &AppError) {
        self.emit("error", Some(error.to_string()), None, None, None);
    }

    fn emit(
        &self,
        stage: &'static str,
        message: Option<String>,
        downloaded_bytes: Option<u64>,
        total_bytes: Option<u64>,
        output_path: Option<String>,
    ) {
        let _ = self.app.emit(
            DOWNLOAD_EVENT,
            DownloadEvent {
                stage,
                message,
                downloaded_bytes,
                total_bytes,
                output_path,
            },
        );
    }
}

struct DownloadPaths {
    target: PathBuf,
    temp: PathBuf,
}

impl DownloadPaths {
    fn new(target: PathBuf) -> Self {
        let temp = partial_path(&target);
        Self { target, temp }
    }
}

struct DownloadPlan {
    resumed_bytes: u64,
    total_bytes: Option<u64>,
    append_mode: bool,
    status_message: String,
}

struct Downloader<'a> {
    client: Client,
    reporter: &'a Reporter<'a>,
    url: String,
    paths: DownloadPaths,
}

impl<'a> Downloader<'a> {
    fn new(reporter: &'a Reporter<'a>, url: String, target: PathBuf) -> AppResult<Self> {
        Ok(Self {
            client: build_client()?,
            reporter,
            url,
            paths: DownloadPaths::new(target),
        })
    }

    fn run(&self) -> AppResult<PathBuf> {
        let remote_total = probe_remote_size(&self.client, &self.url);

        if self.handle_existing_target(remote_total)?
            || self.handle_completed_partial(remote_total)?
        {
            return Ok(self.paths.target.clone());
        }

        let resumed_bytes = self.partial_size()?;
        let mut response = self.request_download(resumed_bytes)?;
        let plan = self.build_plan(&response, remote_total, resumed_bytes);
        let mut file = self.open_partial_file(plan.append_mode)?;

        self.reporter.info(plan.status_message.as_str());
        self.reporter.progress(plan.resumed_bytes, plan.total_bytes);

        let downloaded_bytes = self.copy_response_to_file(
            &mut response,
            &mut file,
            plan.resumed_bytes,
            plan.total_bytes,
        )?;

        self.finalize()?;
        self.reporter.done(
            "下载完成",
            &self.paths.target,
            downloaded_bytes,
            plan.total_bytes,
        );
        Ok(self.paths.target.clone())
    }

    fn handle_existing_target(&self, remote_total: Option<u64>) -> AppResult<bool> {
        if self.paths.target.exists() && !self.paths.temp.exists() {
            let existing_size = file_size(&self.paths.target)?;
            match remote_total {
                Some(total) if total != existing_size => {
                    remove_file(&self.paths.target, "FS-002", "清理旧文件失败")?;
                }
                _ => {
                    self.reporter.done(
                        "文件已存在",
                        &self.paths.target,
                        existing_size,
                        remote_total,
                    );
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    fn handle_completed_partial(&self, remote_total: Option<u64>) -> AppResult<bool> {
        let Some(total) = remote_total else {
            return Ok(false);
        };

        if !self.paths.temp.exists() {
            return Ok(false);
        }

        let partial_size = file_size(&self.paths.temp)?;
        if partial_size == total {
            self.finalize()?;
            self.reporter
                .done("下载完成", &self.paths.target, total, Some(total));
            return Ok(true);
        }

        if partial_size > total {
            remove_file(&self.paths.temp, "FS-003", "清理临时文件失败")?;
        }

        Ok(false)
    }

    fn partial_size(&self) -> AppResult<u64> {
        if self.paths.temp.exists() {
            file_size(&self.paths.temp)
        } else {
            Ok(0)
        }
    }

    fn request_download(&self, resumed_bytes: u64) -> AppResult<Response> {
        let mut request = self.client.get(&self.url);
        if resumed_bytes > 0 {
            request = request.header(RANGE, format!("bytes={resumed_bytes}-"));
        }

        let response = request.send().map_err(|error| {
            AppError::new(
                "NET-002",
                format!(
                    "下载请求失败：{}，地址：{}",
                    describe_reqwest_error(&error),
                    self.url
                ),
            )
        })?;

        if !response.status().is_success() && response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(AppError::new(
                "NET-003",
                format!(
                    "下载被服务器拒绝：HTTP {}，地址：{}",
                    response.status().as_u16(),
                    self.url
                ),
            ));
        }

        Ok(response)
    }

    fn build_plan(
        &self,
        response: &Response,
        remote_total: Option<u64>,
        resumed_bytes: u64,
    ) -> DownloadPlan {
        let can_resume = resumed_bytes > 0 && response.status() == StatusCode::PARTIAL_CONTENT;
        let effective_resumed = if can_resume { resumed_bytes } else { 0 };

        DownloadPlan {
            resumed_bytes: effective_resumed,
            total_bytes: total_bytes_from_response(response, effective_resumed).or(remote_total),
            append_mode: can_resume,
            status_message: if resumed_bytes == 0 {
                "开始下载".to_string()
            } else if can_resume {
                format!("继续下载（{}）", format_bytes(resumed_bytes))
            } else {
                "重新下载".to_string()
            },
        }
    }

    fn open_partial_file(&self, append_mode: bool) -> AppResult<File> {
        if append_mode {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.paths.temp)
                .map_err(|error| {
                    AppError::with_source(
                        "FS-004",
                        format!("无法打开临时文件 {}", self.paths.temp.display()),
                        error,
                    )
                })
        } else {
            File::create(&self.paths.temp).map_err(|error| {
                AppError::with_source(
                    "FS-005",
                    format!("无法创建临时文件 {}", self.paths.temp.display()),
                    error,
                )
            })
        }
    }

    fn copy_response_to_file(
        &self,
        response: &mut Response,
        file: &mut File,
        resumed_bytes: u64,
        total_bytes: Option<u64>,
    ) -> AppResult<u64> {
        let mut downloaded_bytes = resumed_bytes;
        let mut buffer = [0_u8; BUFFER_SIZE];
        let mut last_emit = Instant::now() - PROGRESS_INTERVAL;

        loop {
            let read = response.read(&mut buffer).map_err(|error| {
                AppError::new(
                    "NET-004",
                    format!("读取下载数据失败：{}", describe_io_error(&error)),
                )
            })?;
            if read == 0 {
                break;
            }

            file.write_all(&buffer[..read])
                .map_err(|error| AppError::with_source("FS-006", "写入下载文件失败", error))?;
            downloaded_bytes += read as u64;

            if last_emit.elapsed() >= PROGRESS_INTERVAL {
                self.reporter.progress(downloaded_bytes, total_bytes);
                last_emit = Instant::now();
            }
        }

        let _ = file.flush();
        let _ = file.sync_all();

        if let Some(total) = total_bytes {
            if downloaded_bytes != total {
                return Err(AppError::new(
                    "NET-005",
                    format!(
                        "下载不完整：已下载 {}，预期 {}",
                        format_bytes(downloaded_bytes),
                        format_bytes(total)
                    ),
                ));
            }
        }

        Ok(downloaded_bytes)
    }

    fn finalize(&self) -> AppResult<()> {
        if self.paths.target.exists() {
            remove_file(&self.paths.target, "FS-007", "替换旧文件失败")?;
        }

        fs::rename(&self.paths.temp, &self.paths.target).map_err(|error| {
            AppError::with_source(
                "FS-008",
                format!("写入目标文件失败 {}", self.paths.target.display()),
                error,
            )
        })
    }
}

#[command]
async fn run_install(
    app: AppHandle,
    output_dir: Option<String>,
    output_filename: Option<String>,
    proxy_mode: Option<String>,
    custom_proxy: Option<String>,
) -> Result<InstallResult, String> {
    let config =
        DownloadConfig::from_command(output_dir, output_filename, proxy_mode, custom_proxy)
            .map_err(|error| error.to_string())?;

    let task = tauri::async_runtime::spawn_blocking(move || {
        let reporter = Reporter::new(&app);
        let result = (|| -> AppResult<InstallResult> {
            config.ensure_output_dir()?;
            let url = config.download_url()?;
            let target = config.output_path();

            reporter.info("准备下载");
            let output_path = Downloader::new(&reporter, url, target)?.run()?;

            Ok(InstallResult {
                output_path: output_path.display().to_string(),
            })
        })();

        if let Err(error) = &result {
            reporter.fail(error);
        }

        result
    });

    task.await
        .map_err(|error| AppError::with_source("SYS-001", "下载任务异常退出", error).to_string())?
        .map_err(|error| error.to_string())
}

fn default_output_dir() -> PathBuf {
    dirs::download_dir()
        .map(|path| path.join("GTLite"))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn build_client() -> AppResult<Client> {
    Client::builder()
        .redirect(Policy::limited(10))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36")
        .build()
        .map_err(|error| AppError::with_source("NET-001", "创建下载客户端失败", error))
}

fn apply_proxy_prefix(prefix: &str, source_url: &str) -> String {
    if prefix.contains("{url}") {
        prefix.replace("{url}", source_url)
    } else {
        format!("{}/{}", prefix.trim_end_matches('/'), source_url)
    }
}

fn partial_path(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEFAULT_FILENAME.to_string());
    target.with_file_name(format!("{file_name}.part"))
}

fn probe_remote_size(client: &Client, url: &str) -> Option<u64> {
    let response = client.head(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }

    header_to_u64(&response, CONTENT_LENGTH)
}

fn total_bytes_from_response(response: &Response, resumed_bytes: u64) -> Option<u64> {
    if response.status() == StatusCode::PARTIAL_CONTENT {
        parse_content_range_total(response)
            .or_else(|| header_to_u64(response, CONTENT_LENGTH).map(|value| value + resumed_bytes))
    } else {
        header_to_u64(response, CONTENT_LENGTH)
    }
}

fn parse_content_range_total(response: &Response) -> Option<u64> {
    let header = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let total = header.rsplit('/').next()?;
    if total == "*" {
        return None;
    }

    total.parse().ok()
}

fn header_to_u64(response: &Response, header_name: reqwest::header::HeaderName) -> Option<u64> {
    response
        .headers()
        .get(header_name)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

fn file_size(path: &Path) -> AppResult<u64> {
    path.metadata()
        .map(|metadata| metadata.len())
        .map_err(|error| {
            AppError::with_source(
                "FS-009",
                format!("读取文件信息失败 {}", path.display()),
                error,
            )
        })
}

fn remove_file(path: &Path, code: &'static str, message: &str) -> AppResult<()> {
    fs::remove_file(path).map_err(|error| {
        AppError::with_source(code, format!("{message} {}", path.display()), error)
    })
}

fn describe_reqwest_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "请求超时".to_string();
    }

    if error.is_connect() {
        return format!("连接失败 ({error})");
    }

    if error.is_body() {
        return format!("响应体读取失败 ({error})");
    }

    error.to_string()
}

fn describe_io_error(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::UnexpectedEof => "连接被中断".to_string(),
        std::io::ErrorKind::TimedOut => "读取超时".to_string(),
        _ => error.to_string(),
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];

    let mut value = bytes as f64;
    let mut index = 0;
    while value >= 1024.0 && index < UNITS.len() - 1 {
        value /= 1024.0;
        index += 1;
    }

    if index == 0 {
        format!("{bytes} {}", UNITS[index])
    } else {
        format!("{value:.1} {}", UNITS[index])
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![run_install])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
