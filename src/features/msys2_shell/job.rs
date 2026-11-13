//! Job 管理与命令执行。
//!
//! 每个 job 一个 `std::thread`：线程负责 spawn 子进程、读取合并后的输出、到点杀进程树，
//! 完成后把终态写回共享状态。工具层的 `execute` 只创建 job 并起线程，然后最多等
//! `waitSeconds` 秒；`cancel` 只置"停止等待"标志，**不杀进程**。

use crate::features::msys2_shell::config::BashConfig;
use crate::features::msys2_shell::output::TailBuffer;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// job 的终态类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// 仍在运行。
    Running,
    /// 正常退出（退出码可能为 `None`，例如被外部信号杀掉）。
    Completed,
    /// 启动失败或读取失败。
    Failed,
    /// 总截止时间到点，进程树已被杀掉。
    TimedOut,
    /// 被 `kill_cmd` 杀掉。
    Killed,
}

impl JobStatus {
    /// 状态字符串（与工具契约里的取值一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
            JobStatus::TimedOut => "timed_out",
            JobStatus::Killed => "killed",
        }
    }

    /// 是否已结束。
    pub fn is_finished(self) -> bool {
        !matches!(self, JobStatus::Running)
    }
}

/// 一个 job 的可观测快照（读锁内克隆，锁外再序列化）。
#[derive(Debug, Clone)]
pub struct JobView {
    /// job id，从 1 开始。
    pub id: u64,
    /// 原始命令。
    pub command: String,
    /// 当前状态。
    pub status: JobStatus,
    /// 退出码；未结束或被杀时为 `None`。
    pub exit_code: Option<i32>,
    /// 启动时间（Unix 毫秒）。
    pub started_at_millis: u64,
    /// 已运行毫秒数（运行中为实时值，已结束为总耗时）。
    pub elapsed_millis: u64,
    /// 输出正文（已按上限截断并解码）。
    pub output: String,
    /// 是否发生了截断。
    pub truncated: bool,
    /// 失败原因（仅 `Failed`）。
    pub error: Option<String>,
}

/// job 的内部共享状态。
#[derive(Debug)]
struct JobState {
    /// job 序号（便于诊断，与 id 相同）。
    id: u64,
    /// 原始命令。
    command: String,
    /// 当前状态。
    status: JobStatus,
    /// 退出码。
    exit_code: Option<i32>,
    /// 启动时刻。
    started_at: Instant,
    /// 启动时刻（绝对时间，用于展示）。
    started_at_millis: u64,
    /// 结束时刻。
    finished_at: Option<Instant>,
    /// 合并后的输出缓冲。
    output: TailBuffer,
    /// 失败原因。
    error: Option<String>,
    /// 子进程 PID（0 表示还没起来或已回收）。
    pid: u32,
}

impl JobState {
    /// 构造快照。
    fn view(&self) -> JobView {
        let elapsed = self
            .finished_at
            .unwrap_or_else(Instant::now)
            .duration_since(self.started_at);
        JobView {
            id: self.id,
            command: self.command.clone(),
            status: self.status,
            exit_code: self.exit_code,
            started_at_millis: self.started_at_millis,
            elapsed_millis: elapsed.as_millis() as u64,
            output: self.output.render(),
            truncated: self.output.is_truncated(),
            error: self.error.clone(),
        }
    }
}

/// 单个 job 的句柄：状态 + 停止等待标志。
pub struct Job {
    /// 共享状态（job 线程也会写）。
    state: Arc<Mutex<JobState>>,
    /// `true` 表示调用方不再需要等待（工具层 cancel），**不影响进程**。
    stop_waiting: Arc<AtomicBool>,
}

impl Job {
    /// 共享状态。
    fn state(&self) -> MutexGuard<'_, JobState> {
        // 中毒时取回内部值继续用：单个 job 失败不应连累整个 manager。
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 置“停止等待”标志。幂等，不杀进程。
    ///
    /// 当前取消链路走 `ToolHandles` 的 `cancelled` 标志（见 [`crate::features::msys2_shell`]），
    /// 这里保留该方法供非工具入口（例如未来的 CLI）使用。
    #[allow(dead_code)]
    pub fn stop_waiting(&self) {
        self.stop_waiting.store(true, Ordering::SeqCst);
    }

    /// 是否已被要求停止等待。
    pub fn should_stop_waiting(&self) -> bool {
        self.stop_waiting.load(Ordering::SeqCst)
    }

    /// 当前快照。
    pub fn view(&self) -> JobView {
        self.state().view()
    }

    /// 当前状态。
    pub fn status(&self) -> JobStatus {
        self.state().status
    }
}

/// Job 管理器：分配 id、保留已完成结果、执行 kill。
pub struct JobManager {
    /// 下一个 job id。
    next_id: AtomicU64,
    /// 所有活跃 job（运行中 + 保留的已完成），按 id 升序。
    jobs: Vec<Arc<Job>>,
    /// 保留的已结束 job 条数上限。
    max_jobs: usize,
}

impl JobManager {
    /// 创建管理器。
    ///
    /// 参数：`max_jobs` 已结束 job 的保留条数（>= 1，配置层已校验）。
    pub fn new(max_jobs: usize) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            jobs: Vec::new(),
            max_jobs: max_jobs.max(1),
        }
    }

    /// 分配一个新 job 并登记。
    ///
    /// 参数：`command` 原始命令；`max_output_bytes` 单流输出上限。
    /// 返回：job 句柄 + 分配到的 id。
    fn register(&mut self, command: &str, max_output_bytes: usize) -> (Arc<Job>, u64) {
        // 第一步：分配 id（从 1 开始，单调递增）。
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // 第二步：构造共享状态。
        let state = JobState {
            id,
            command: command.to_string(),
            status: JobStatus::Running,
            exit_code: None,
            started_at: Instant::now(),
            started_at_millis: now_millis(),
            finished_at: None,
            output: TailBuffer::new(max_output_bytes),
            error: None,
            pid: 0,
        };
        let job = Arc::new(Job {
            state: Arc::new(Mutex::new(state)),
            stop_waiting: Arc::new(AtomicBool::new(false)),
        });

        // 第三步：登记并按保留策略淘汰旧结果。
        self.jobs.push(Arc::clone(&job));
        self.prune();
        (job, id)
    }

    /// 淘汰超额的已结束 job；运行中的始终保留。
    fn prune(&mut self) {
        // 第一步：统计已结束的条数。
        let finished = self
            .jobs
            .iter()
            .filter(|job| job.status().is_finished())
            .count();
        if finished <= self.max_jobs {
            return;
        }

        // 第二步：从最旧开始删，直到回到上限（运行中的跳过）。
        let mut to_remove = finished - self.max_jobs;
        self.jobs.retain(|job| {
            if to_remove > 0 && job.status().is_finished() {
                to_remove -= 1;
                return false;
            }
            true
        });
    }

    /// 按 id 查 job。读之前先淘汰超额结果，保证 `maxJobs` 对自然结束的 job 也生效。
    pub fn get(&mut self, id: u64) -> Option<Arc<Job>> {
        self.prune();
        self.jobs.iter().find(|job| job.state().id == id).cloned()
    }

    /// 列出所有 job（运行中 + 保留的已完成），按 id 升序。
    pub fn list(&mut self) -> Vec<JobView> {
        self.prune();
        self.jobs.iter().map(|job| job.view()).collect()
    }

    /// 杀掉指定 job 的进程树。
    ///
    /// 参数：`id` job id。
    /// 返回：
    /// - `Ok(Some(status))`：刚被置为 `Killed`。
    /// - `Ok(None)`：job 已结束，属于幂等 no-op（不报错）。
    /// - `Err(reason)`：id 不存在。
    ///
    /// 只杀进程树，**不**移除 job：被杀前的部分输出仍可用 `get_cmd_result` 取回。
    pub fn kill(&mut self, id: u64) -> Result<Option<JobStatus>, String> {
        let Some(job) = self.get(id) else {
            return Err(format!("no such job: {id}"));
        };
        let mut state = job.state();

        // 第一步：已结束就是 no-op（kill 幂等）。
        if state.status.is_finished() {
            return Ok(None);
        }

        // 第二步：杀进程树，再标记终态。
        let pid = state.pid;
        if pid > 0 {
            kill_process_tree(pid);
        }
        state.status = JobStatus::Killed;
        state.exit_code = None;
        state.finished_at = Some(Instant::now());
        drop(state);

        // 第三步：终止后同步一次保留策略。
        self.prune();
        Ok(Some(JobStatus::Killed))
    }

    /// 起线程执行一条新命令。
    ///
    /// 参数：
    /// - `config`：已校验配置。
    /// - `command`：命令文本。
    /// - `cwd`：工作目录（已解析为 Windows 路径）。
    /// - `timeout_seconds`：本次 job 的总截止秒数（可覆盖配置默认值）。
    ///
    /// 返回：job 句柄 + id。
    pub fn spawn(
        &mut self,
        config: &BashConfig,
        command: &str,
        cwd: &Path,
        timeout_seconds: u64,
    ) -> (Arc<Job>, u64) {
        let (job, id) = self.register(command, config.max_output_bytes);
        // 线程拿到必要的克隆，配置本体不进入线程（避免无谓的整份复制）。
        let state = Arc::clone(&job.state);
        let shell = config.shell.clone();
        let msystem = config.msystem.clone();
        // 命令行与注入环境在启动期已定，这里只搬走需要的东西。
        let login_shell = config.login_shell;
        let login_env = config.login_env.vars().clone();
        let command = command.to_string();
        let cwd = cwd.to_path_buf();
        let timeout = Duration::from_secs(timeout_seconds);

        let spawned = std::thread::Builder::new()
            .name(format!("msys2-bash-{id}"))
            .spawn(move || {
                run_job(
                    state, shell, msystem, login_shell, login_env, command, cwd, timeout,
                )
            });

        if let Err(error) = spawned {
            // 线程起不来属于硬失败：直接落终态，不让 job 永远停在 running。
            let mut state = job.state();
            state.status = JobStatus::Failed;
            state.error = Some(format!("failed to spawn job thread: {error}"));
            state.finished_at = Some(Instant::now());
        }
        (job, id)
    }
}

/// 全局 manager（配置在加载期确定，`OnceLock` 足够）。
static MANAGER: OnceLock<Mutex<JobManager>> = OnceLock::new();

/// 取得全局 manager 的互斥访问。
///
/// 返回：`None` 表示 manager 尚未初始化（未启用本 feature）。
pub fn manager() -> Option<&'static Mutex<JobManager>> {
    MANAGER.get()
}

/// 初始化全局 manager。
///
/// 参数：`max_jobs` 已结束 job 保留条数。
/// 返回：`true` 表示本次完成初始化；`false` 表示已经初始化过（重复调用无副作用）。
pub fn init_manager(max_jobs: usize) -> bool {
    MANAGER.set(Mutex::new(JobManager::new(max_jobs))).is_ok()
}

/// 当前 Unix 毫秒时间戳（失败时退回 0，仅影响展示）。
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 用 `taskkill /F /T` 杀掉整个进程树。
///
/// 参数：`pid` 目标进程 PID。
/// 返回：无。失败被忽略——进程可能已自行退出，`kill` 语义要求幂等。
fn kill_process_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// job 线程主体：spawn 子进程 → 读合并输出 → 到点杀树 → 写终态。
///
/// 参数：
/// - `state`：共享状态（本线程是唯一写入者）。
/// - `shell`/`msystem`：shell 路径与 MSYSTEM 值。
/// - `login_shell`：`true` 用 `-lc`（source /etc/profile）；`false` 用 `-c` + 注入环境。
/// - `login_env`：启动期捕获的环境变量（`login_shell=true` 时为空）。
/// - `command`/`cwd`/`timeout`：执行参数。
///
/// 注意：本线程**不**看“停止等待”标志——那个标志只影响工具层还要不要继续等，
/// 进程必须一直跑到自然结束或被 `kill_cmd`/超时杀掉。
fn run_job(
    state: Arc<Mutex<JobState>>,
    shell: PathBuf,
    msystem: String,
    login_shell: bool,
    login_env: std::collections::HashMap<String, String>,
    command: String,
    cwd: PathBuf,
    timeout: Duration,
) {
    // 第一步：构造子进程。stdout/stderr 都走 piped，稍后由两个线程分别读取。
    //
    // 关键性能选择：默认用 `-c`（非 login）而不是 `-lc`。MSYS2 的 login shell
    // 每次都要 source /etc/profile，实测单次命令多花约 540~640ms；而 `-c` 配合
    // 启动期注入的环境变量能得到同样的 PATH / 自定义变量（见 env_cache 注释）。
    let mut builder = Command::new(&shell);
    builder
        .arg(if login_shell { "-lc" } else { "-c" })
        .arg(&command)
        .current_dir(&cwd)
        .env("MSYSTEM", &msystem)
        .env("CHERE_INVOKING", "1")
        .env("LANG", "C.UTF-8");
    // 注入启动期捕获的环境变量（登录模式不需要，profile 会自己建好）。
    if !login_shell && !login_env.is_empty() {
        builder.envs(&login_env);
    }
    builder
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
            state.status = JobStatus::Failed;
            state.error = Some(format!("failed to start shell: {error}"));
            state.finished_at = Some(Instant::now());
            return;
        }
    };

    // 第二步：记下 PID，供 kill_cmd / 超时使用。
    let pid = child.id();
    {
        let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
        state.pid = pid;
    }
    // 第三步：stdout 与 stderr 各起一个读取线程，合并写入同一个缓冲。
    let stdout_reader = child.stdout.take().map(|stdout| spawn_reader(stdout, Arc::clone(&state)));
    let stderr_reader = child.stderr.take().map(|stderr| spawn_reader(stderr, Arc::clone(&state)));

    // 第四步：等待退出，超时则杀进程树。
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let exit_status: Option<std::process::ExitStatus>;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    // 到点：杀整棵树，让读取线程尽快看到 EOF。
                    if pid > 0 {
                        kill_process_tree(pid);
                    }
                    timed_out = true;
                    // 再等一次，尽量回收退出状态（进程可能已死）。
                    exit_status = child.wait().ok();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
                state.status = JobStatus::Failed;
                state.error = Some(format!("failed to wait for shell: {error}"));
                state.finished_at = Some(Instant::now());
                return;
            }
        }
    }

    // 第五步：等读取线程收尾，确保输出完整（否则短命令会丢尾巴）。
    for reader in [stdout_reader, stderr_reader].into_iter().flatten() {
        let _ = reader.join();
    }

    // 第六步：写终态。已被 kill_cmd 标记为 Killed 时不覆盖（先到者胜）。
    let exit_code = exit_status.and_then(|status| status.code());
    let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
    if state.status == JobStatus::Killed {
        return;
    }
    state.exit_code = exit_code;
    state.status = if timed_out {
        JobStatus::TimedOut
    } else if exit_status.is_some() {
        JobStatus::Completed
    } else {
        JobStatus::Failed
    };
    if state.status == JobStatus::Failed && state.error.is_none() {
        state.error = Some("shell exited without a status code".to_string());
    }
    state.finished_at = Some(Instant::now());
}

/// 起一个读取线程，把该流的所有字节追加进 job 输出缓冲。
///
/// 参数：`reader` 子进程的输出管道；`state` 共享状态。
/// 返回：线程句柄（调用方 join 以确认输出已收全）。
fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    state: Arc<Mutex<JobState>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => {
                    let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
                    state.output.append(&buffer[..read]);
                }
                Err(_) => return,
            }
        }
    })
}

/// 解析工具传入的 `cwd`。
///
/// 参数：
/// - `requested`：工具参数里的 `cwd`（可为空）。
/// - `host_cwd`：宿主注入的 `__rpi.cwd`（可为空）。
/// - `fallback`：两者都没有时的兜底目录。
///
/// 返回：`Ok(Windows 绝对路径)`；目录不存在或不是目录时 `Err(原因)`。
///
/// 支持 MSYS2 风格路径：`/c/Users/x` → `C:\Users\x`；`/tmp` 之类内部路径按
/// `msys2_root` 解析（`C:\msys64\tmp`）。
pub fn resolve_cwd(
    requested: Option<&str>,
    host_cwd: Option<&str>,
    fallback: &Path,
    msys2_root: &Path,
) -> Result<PathBuf, String> {
    // 第一步：按优先级取第一个非空路径。
    let raw = requested
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .or_else(|| host_cwd.map(str::trim).filter(|text| !text.is_empty()))
        .map(str::to_string);

    let Some(raw) = raw else {
        // 没有任何来源：用兜底目录（进程当前目录），它一定存在。
        return Ok(fallback.to_path_buf());
    };

    // 第二步：MSYS2 风格路径需要转换，Windows 原生路径原样使用。
    let converted = if raw.starts_with('/') {
        msys2_to_windows(&raw, msys2_root)
    } else {
        PathBuf::from(&raw)
    };

    // 第三步：校验目录可用性。这里用「存在且是目录」判据，不额外做权限探测。
    if !converted.is_dir() {
        return Err(format!("cwd is not a directory: {}", converted.display()));
    }
    Ok(converted)
}

/// 把 MSYS2 风格路径转成 Windows 原生路径。
///
/// 参数：`path` 以 `/` 开头的路径；`msys2_root` MSYS2 根目录。
/// 返回：
/// - `/c/Users/x` → `C:\Users\x`
/// - `/tmp`、`/home/x` 等内部路径 → `<msys2_root>\tmp`、`<msys2_root>\home\x`
pub fn msys2_to_windows(path: &str, msys2_root: &Path) -> PathBuf {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');

    // 第一步：判断是否形如 `<盘符>/...`（单个字母后跟 `/`）。
    let mut chars = trimmed.chars();
    if let (Some(drive), Some('/')) = (chars.next(), chars.next()) {
        if drive.is_ascii_alphabetic() {
            let rest: String = chars.collect();
            let rest = rest.replace('/', "\\");
            return if rest.is_empty() {
                PathBuf::from(format!("{}:\\", drive.to_ascii_uppercase()))
            } else {
                PathBuf::from(format!("{}:\\{}", drive.to_ascii_uppercase(), rest))
            };
        }
    }

    // 第二步：内部虚拟路径：挂到 msys2_root 下。
    if trimmed.is_empty() {
        return msys2_root.to_path_buf();
    }
    msys2_root.join(trimmed.replace('/', "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        // 用当前测试可执行文件所在目录充当"存在的目录"。
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .expect("test dir")
    }

    #[test]
    fn twenty_five_finished_jobs_keep_only_twenty() {
        // 对应验收 §10.14：maxJobs=20，起 25 条已结束命令，只保留最近 20 条。
        let mut manager = JobManager::new(20);
        for index in 1..=25 {
            let (job, _id) = manager.register(&format!("echo job{index}"), 1024);
            let mut state = job.state();
            state.status = JobStatus::Completed;
            state.exit_code = Some(0);
            state.finished_at = Some(Instant::now());
        }

        let listed = manager.list();
        assert_eq!(listed.len(), 20, "must retain exactly maxJobs finished jobs");
        // 保留的必须是最新的 20 条（id 6..=25），最旧的 1..=5 已淘汰。
        assert_eq!(listed.first().map(|view| view.id), Some(6));
        assert_eq!(listed.last().map(|view| view.id), Some(25));
    }

    #[test]
    fn many_finished_jobs_keep_every_running_one() {
        // 运行中的 job 永远不因 maxJobs 被淘汰。
        let mut manager = JobManager::new(3);
        let (_running, running_id) = manager.register("sleep 999", 1024);
        for _ in 0..10 {
            let (job, _id) = manager.register("echo x", 1024);
            let mut state = job.state();
            state.status = JobStatus::Completed;
            state.finished_at = Some(Instant::now());
        }
        let listed = manager.list();
        assert_eq!(listed.len(), 4, "3 finished + 1 running");
        assert!(listed.iter().any(|view| view.id == running_id));
    }

    #[test]
    fn cancel_stops_waiting_but_keeps_the_job_queryable() {
        // 对应验收 §10.12：取消一次 bash 等待后，job 仍在 manager 里可查。
        let mut manager = JobManager::new(20);
        let (job, id) = manager.register("sleep 300", 1024);

        // cancel 只置取消位：状态仍是 running，进程不受影响。
        manager
            .get(id)
            .expect("job must still exist")
            .stop_waiting();
        assert_eq!(job.status(), JobStatus::Running, "cancel must not kill the job");

        // 取消后 job 依旧能按 id 查到（这就是 get_cmd_result 的取数路径）。
        let found = manager.get(id).expect("job must remain queryable after cancel");
        assert_eq!(found.status(), JobStatus::Running);

        // 模拟进程自然结束：结果仍可取回。
        {
            let mut state = job.state();
            state.status = JobStatus::Completed;
            state.exit_code = Some(0);
            state.output.append(b"late result\n");
            state.finished_at = Some(Instant::now());
        }
        let view = manager.get(id).expect("still there").view();
        assert_eq!(view.status, JobStatus::Completed);
        assert!(view.output.contains("late result"));
    }

    #[test]
    fn ids_start_at_one_and_increase() {
        let mut manager = JobManager::new(20);
        let (_a, first) = manager.register("echo a", 1024);
        let (_b, second) = manager.register("echo b", 1024);
        assert_eq!(first, 1);
        assert_eq!(second, 2);
    }

    #[test]
    fn finished_jobs_are_pruned_but_running_ones_are_kept() {
        let mut manager = JobManager::new(2);
        let (_keep_running, running_id) = manager.register("sleep 1", 1024);
        for index in 0..5 {
            let (job, _id) = manager.register("echo x", 1024);
            let mut state = job.state();
            state.status = JobStatus::Completed;
            state.finished_at = Some(Instant::now());
            let _ = index;
        }
        // 只保留最近 2 条已结束，运行中的那条还在。
        assert_eq!(manager.list().len(), 3);
        assert!(manager.get(running_id).is_some());
        let finished = manager
            .list()
            .into_iter()
            .filter(|view| view.status.is_finished())
            .count();
        assert_eq!(finished, 2);
    }

    #[test]
    fn kill_is_idempotent_and_returns_none_for_finished_jobs() {
        let mut manager = JobManager::new(20);
        let (job, id) = manager.register("echo x", 1024);
        {
            let mut state = job.state();
            state.status = JobStatus::Completed;
            state.finished_at = Some(Instant::now());
        }
        assert_eq!(manager.kill(id).expect("known id"), None);
        assert!(manager.kill(9999).is_err());
    }

    #[test]
    fn stop_waiting_never_touches_the_status() {
        let (_job_ref, _id) = {
            let mut manager = JobManager::new(20);
            let (job, id) = manager.register("sleep 5", 1024);
            // cancel 只置标志，状态仍是 running（进程由 kill_cmd 负责）。
            job.stop_waiting();
            assert!(job.should_stop_waiting());
            assert_eq!(job.status(), JobStatus::Running);
            (job, id)
        };
    }

    #[test]
    fn msys2_paths_convert_to_windows_paths() {
        let root = Path::new(r"C:\msys64");
        assert_eq!(
            msys2_to_windows("/c/Users/test", root),
            PathBuf::from(r"C:\Users\test")
        );
        assert_eq!(
            msys2_to_windows("/d/data/files", root),
            PathBuf::from(r"D:\data\files")
        );
        assert_eq!(msys2_to_windows("/tmp", root), PathBuf::from(r"C:\msys64\tmp"));
        assert_eq!(msys2_to_windows("/", root), PathBuf::from(r"C:\msys64"));
    }

    #[test]
    fn resolve_cwd_prefers_requested_then_host_then_fallback() {
        let dir = temp_root();
        let root = Path::new(r"C:\msys64");
        let requested = dir.display().to_string();

        let resolved = resolve_cwd(Some(&requested), Some(r"C:\nope"), &dir, root).expect("ok");
        assert_eq!(resolved, dir);

        // 只给 host_cwd 时用它（这里给一个存在的目录）。
        let resolved = resolve_cwd(Some("  "), Some(&requested), &dir, root).expect("ok");
        assert_eq!(resolved, dir);

        // 都没有 → 兜底。
        let resolved = resolve_cwd(None, None, &dir, root).expect("ok");
        assert_eq!(resolved, dir);
    }

    #[test]
    fn resolve_cwd_rejects_missing_directory() {
        let dir = temp_root();
        let root = Path::new(r"C:\msys64");
        let missing = r"C:\definitely-not-here-9f3a";
        assert!(resolve_cwd(Some(missing), None, &dir, root).is_err());
    }

    #[test]
    fn tail_buffer_is_reachable_through_job_state() {
        // 保证 job 与 output 的接口对齐（编译期即验证）。
        let mut manager = JobManager::new(4);
        let (job, _id) = manager.register("echo", 8);
        {
            let mut state = job.state();
            state.output.append(b"0123456789ABCDEF");
            state.status = JobStatus::Completed;
            state.finished_at = Some(Instant::now());
        }
        let view = job.view();
        assert!(view.truncated);
        assert!(view.output.contains("[truncated 8 bytes]"), "{}", view.output);
        assert!(view.output.ends_with("89ABCDEF"), "{}", view.output);
    }
}
