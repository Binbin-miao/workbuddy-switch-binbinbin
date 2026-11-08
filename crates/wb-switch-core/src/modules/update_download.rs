//! 更新包下载加速：GitHub 镜像改写 + Range 分段并发 + 签名校验。
//!
//! 为什么不用 `tauri-plugin-updater` 自带的 `Update::download`：它是一条连接从头拉到尾，
//! 国内直连 github.com 常年几十 KB/s，甚至直接超时。插件把 `download_url` / `signature`
//! 都暴露成 pub 字段，`install(bytes)` 也接受外部字节，所以这里自己下载、自己验签，
//! 再把同一份字节交给 `install`，安装语义与插件完全一致。
//!
//! 三层保障：
//! 1. **镜像优先**：前缀改写 GitHub 下载地址，直连放最后兜底；镜像请求显式
//!    `no_proxy`，不受系统代理影响（否则会被带去海外节点，比直连还慢）；
//! 2. **分段并发**：4 路 Range 请求并行拉取，按「空闲超时」而不是总超时判死，
//!    避免慢速但可用的线路被误杀；
//! 3. **签名兜底**：任何一个镜像返回脏数据 / 半截文件，都会在验签这步被拒，
//!    绝不可能装进用户机器。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;

use crate::modules::config::DEFAULT_HTTP_USER_AGENT;

/// 进度回调：`(累计已下载字节, 总字节)`。多路分段共用一个回调，调用方需自行去抖。
pub type Progress = Arc<dyn Fn(u64, Option<u64>) + Send + Sync>;

/// 用户主动取消时的错误文案。宿主拿它区分「取消」与「通道失败」。
pub const CANCELLED_ERROR: &str = "已取消下载";

/// 通道被判涓流 / 超预算时的错误文案；语义是「换个源」，不是「更新失败」。
pub const STALLED_ERROR: &str = "下载速度过低，已切换下载源";

/// 一次下载阶段的共享控制块：取消标志、停滞标志、累计字节与起始时刻。
///
/// 宿主创建 `Arc<DownloadControl>` 后同时交给下载流程与取消命令；core 只负责两件事：
/// 每个检查点读标志，每收到一块字节就累加到 `downloaded`（宿主据此做滑动窗口采样）。
pub struct DownloadControl {
    cancelled: AtomicBool,
    stalled: AtomicBool,
    downloaded: AtomicU64,
    started: Instant,
}

impl DownloadControl {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            stalled: AtomicBool::new(false),
            downloaded: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    /// 用户点了「取消下载」：幂等，重复置位不改变任何状态。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// 宿主判定当前候选涓流 / 超预算时置位，core 会在下一个检查点放弃这条通道。
    ///
    /// `reason` 只进日志：用户看到的固定是 [`STALLED_ERROR`]，换源成功就不该让他看见抖动。
    pub fn stall(&self, reason: &str) {
        eprintln!("[更新] 判定下载停滞：{reason}");
        self.stalled.store(true, Ordering::SeqCst);
    }

    pub fn is_stalled(&self) -> bool {
        self.stalled.load(Ordering::SeqCst)
    }

    /// 累计已下载字节，供宿主的滑动窗口采样。
    pub fn record_bytes(&self, bytes: u64) {
        self.downloaded.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 失败重试前把多算的字节退回，否则重试会让进度虚高。
    pub fn release_bytes(&self, bytes: u64) {
        self.downloaded.fetch_sub(bytes, Ordering::Relaxed);
    }

    pub fn downloaded(&self) -> u64 {
        self.downloaded.load(Ordering::Relaxed)
    }

    /// 换候选时进度必须从 0 重新累计（[D5]），否则会出现「只下了 15% 却显示 100%」。
    pub fn reset_progress(&self) {
        self.downloaded.store(0, Ordering::Relaxed);
    }

    /// 下载开始至今的时长，宿主用它判断是否还在预热期内。
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// 检查点：命中取消或停滞立即返回。所有 `send()` 之前、每一轮轮询都要过一遍。
    pub fn check(&self) -> Result<(), String> {
        if self.is_cancelled() {
            return Err(CANCELLED_ERROR.to_string());
        }
        if self.is_stalled() {
            return Err(STALLED_ERROR.to_string());
        }
        Ok(())
    }
}

impl Default for DownloadControl {
    fn default() -> Self {
        Self::new()
    }
}

/// 是不是取消 / 停滞这两个「控制类」失败：它们绝不能当网络错误重试。
fn is_control_error(error: &str) -> bool {
    error == CANCELLED_ERROR || error == STALLED_ERROR
}

/// 窗口吞吐判据（纯函数，可单测）：最近 `elapsed` 内只下了 `bytes` 字节，算不算涓流。
///
/// 为什么需要它：`IDLE_TIMEOUT` 判的是「零字节」，而限速镜像每秒仍在回几 KB，
/// 于是永远不判死、也不换源（实测 gh-proxy.com 16.7 KB/s 卡在 85% 再也不动）。
///
/// 四条显式边界，顺序即优先级：
/// 1. `total` 未知 → 不判：无法排除「正常慢速的小文件」，只受候选预算约束；
/// 2. 已下满（`bytes >= total`）→ 不判：100% 之后速度归零是正常收尾；
/// 3. `elapsed < STALL_WARMUP` → 不判：建链与首包本来就没有吞吐；
/// 4. 其余按 `bytes * 1000 / elapsed_ms < MIN_THROUGHPUT` 判涓流。
pub fn throughput_is_stalled(elapsed: Duration, bytes: u64, total: Option<u64>) -> bool {
    let Some(total) = total else {
        return false;
    };
    // 第 2 条边界：本意是「已下满 100% 不判」。注意宿主传进来的 `bytes` 是**窗口增量**，
    // 不是累计量，所以本分支在宿主路径上是纯兜底、实际不触发 —— 别据此推断参数语义。
    if bytes >= total {
        return false;
    }
    if elapsed < STALL_WARMUP {
        return false;
    }
    let elapsed_ms = elapsed.as_millis().max(1) as u64;
    bytes * 1000 / elapsed_ms < MIN_THROUGHPUT
}

/// 内置 GitHub 镜像前缀（按实测速度排序，直连放最后）。
///
/// 只放实测过「能下完整文件 + 支持 Range」的站点；镜像挂掉时自动降级，
/// 不会影响更新成功率。
pub const MIRROR_PREFIXES: [&str; 2] = ["https://gh-proxy.com", "https://ghproxy.net"];

/// 分段并发数。安装包约 8.5 MB，4 路已能跑满带宽；再多容易被镜像限流。
pub const SEGMENT_CONNECTIONS: usize = 4;

/// 小于这个体积不值得分段（小文件握手开销比并发收益大）。
const MIN_SEGMENT_BYTES: u64 = 1 << 20;

/// 建连超时。走代理时不可达要快速失败，别让用户干等。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 镜像直连的建连超时（更短：镜像不通就马上换下一个候选）。
const MIRROR_CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

/// 空闲超时：连续这么久**一个字节都没收到**就判定这条连接已死。
///
/// 慢于速度地板（80 KB/s）的通道由涓流判据负责换源，不归这里管：本超时只负责判死
/// 完全停住的连接。用「空闲」而不是「总时长」，才能让慢速线路跑到最后一块。
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// 速度地板的滑动窗口：判涓流看的是「最近这么久内的平均吞吐」，不是整段均速。
pub const STALL_WINDOW: Duration = Duration::from_secs(15);

/// 速度地板：窗口内平均吞吐低于这个值即判涓流。
/// 80 KB/s 是「8.5 MB 包两分钟内下完」的下限，实测正常镜像 316 KB/s、限速镜像 16.7 KB/s。
pub const MIN_THROUGHPUT: u64 = 80 * 1024;

/// 预热期：刚开始的建链与首包没有吞吐，不该被速度地板误杀。
pub const STALL_WARMUP: Duration = Duration::from_secs(5);

/// 单个候选的预算：到点判死换下一个候选，别让一条通道拖住整条下载。
pub const CANDIDATE_BUDGET: Duration = Duration::from_secs(150);

/// 整条下载阶段的总预算（含测速、换源、验签）。
pub const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(360);

/// 取消 / 停滞的检查点间隔：取消命令的响应上界就是它。
pub const CANCEL_POLL: Duration = Duration::from_millis(500);

/// 单个分片的重试次数（不含首次）。重试只针对同一分片，不会从头再来。
const SEGMENT_RETRIES: usize = 2;

/// 探测请求（`Range: bytes=0-0`）的超时。
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// 把 GitHub 下载地址改写成镜像地址；前缀为空时原样返回。
pub fn mirror_url(url: &str, prefix: &str) -> String {
    let prefix = prefix.trim().trim_end_matches('/');
    if prefix.is_empty() {
        return url.to_string();
    }
    format!("{prefix}/{url}")
}

/// 候选下载地址：先镜像、后直连。非 GitHub 地址不做改写（第三方自建更新源）。
pub fn download_candidates(url: &str) -> Vec<String> {
    if !url.starts_with("https://github.com/") {
        return vec![url.to_string()];
    }
    let mut candidates: Vec<String> = MIRROR_PREFIXES
        .iter()
        .map(|prefix| mirror_url(url, prefix))
        .collect();
    candidates.push(url.to_string());
    candidates.dedup();
    candidates
}

/// 测速样本大小：1.5 MiB。256 KiB 会被「首包突发」骗过，把限速镜像排到第一。
const PROBE_SAMPLE_BYTES: u64 = 1536 * 1024;

/// 单个候选的测速预算：拿满样本会立即返回，只有慢/死的通道才吃满这 5 秒。
const PROBE_BUDGET: Duration = Duration::from_secs(5);

/// 测速阶段的建连超时：比正式下载短得多——不通就赶紧换下一条。
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_millis(1_500);

/// 按**实测速度**重排候选：快的在前，没测通的沉底但仍保留（兜底）。
///
/// 为什么不能写死顺序：镜像与直连谁更快完全取决于用户网络。实测过两种相反的环境——
/// 有的机器镜像 6.7 MB/s、直连 0；有的机器镜像 20 KB/s、代理直连 3.5 MB/s。
/// 写死任何一边都会让另一半用户慢几十倍，所以排序必须是运行时结果。
///
/// 并发探测、互不影响；单条失败只意味着它拿 0 分，不会中断整体。
pub async fn rank_candidates(candidates: &[String], proxy: Option<&str>) -> Vec<String> {
    if candidates.len() <= 1 {
        return candidates.to_vec();
    }
    let mut set = tokio::task::JoinSet::new();
    for (index, url) in candidates.iter().enumerate() {
        let url = url.clone();
        let proxy = proxy.map(str::to_string);
        set.spawn(async move {
            let speed = probe_speed(&url, proxy.as_deref()).await;
            (index, speed)
        });
    }
    // 按索引回填而不是收集返回值：即使某条任务被取消，候选也不会从列表里消失。
    let mut speeds = vec![0u64; candidates.len()];
    while let Some(joined) = set.join_next().await {
        if let Ok((index, speed)) = joined {
            speeds[index] = speed;
        }
    }
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by(|&a, &b| speeds[b].cmp(&speeds[a]).then(a.cmp(&b)));
    order.into_iter().map(|index| candidates[index].clone()).collect()
}

/// 小样本测速：返回字节/秒；任何失败都返回 0（等价于「这条最慢」）。
async fn probe_speed(url: &str, proxy: Option<&str>) -> u64 {
    let direct = is_mirror_url(url);
    let mut builder = reqwest::Client::builder()
        .user_agent(DEFAULT_HTTP_USER_AGENT)
        .connect_timeout(PROBE_CONNECT_TIMEOUT);
    if direct {
        builder = builder.no_proxy();
    } else if let Some(proxy) = proxy.map(str::trim).filter(|value| !value.is_empty()) {
        match reqwest::Proxy::all(proxy) {
            Ok(configured) => builder = builder.proxy(configured),
            Err(_) => return 0,
        }
    }
    let Ok(client) = builder.build() else {
        return 0;
    };
    let started = std::time::Instant::now();
    let Ok(response) = client
        .get(url)
        .header(
            reqwest::header::RANGE,
            format!("bytes=0-{}", PROBE_SAMPLE_BYTES - 1),
        )
        .timeout(PROBE_BUDGET)
        .send()
        .await
    else {
        return 0;
    };
    let status = response.status();
    if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return 0;
    }
    // 被墙时镜像常返回一个 200 的 HTML 拦截页：按 0 字节处理，别让它蒙到高分。
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"))
    {
        return 0;
    }
    let mut response = response;
    let mut received: u64 = 0;
    loop {
        if received >= PROBE_SAMPLE_BYTES || started.elapsed() >= PROBE_BUDGET {
            break;
        }
        match tokio::time::timeout(PROBE_BUDGET, response.chunk()).await {
            Ok(Ok(Some(chunk))) => received += chunk.len() as u64,
            _ => break,
        }
    }
    let elapsed_ms = started.elapsed().as_millis().max(1) as u64;
    received * 1000 / elapsed_ms
}

/// 把总字节切成 `connections` 段，返回闭区间 `(start, end)`。
pub fn plan_segments(total: u64, connections: usize) -> Vec<(u64, u64)> {
    if total == 0 || connections == 0 {
        return Vec::new();
    }
    let chunk = total.div_ceil(connections as u64);
    let mut segments = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + chunk).min(total) - 1;
        segments.push((start, end));
        start = end + 1;
    }
    segments
}

/// 解析 `Content-Range: bytes 0-0/8815103` 里的总长度。
fn parse_content_range(value: &str) -> Option<u64> {
    let (_, total) = value.rsplit_once('/')?;
    total.trim().parse::<u64>().ok()
}

/// 校验更新包签名（minisign，与 `tauri-plugin-updater` 同一套）。
///
/// 公钥与签名在配置里都是「base64 编码的 minisign 文本」，这里按同样方式先解一层 base64。
pub fn verify_signature(data: &[u8], signature: &str, pubkey: &str) -> Result<(), String> {
    let public_key = minisign_verify::PublicKey::decode(&decode_base64(pubkey, "更新公钥")?)
        .map_err(|error| format!("更新公钥无效：{error}"))?;
    let signature = minisign_verify::Signature::decode(&decode_base64(signature, "更新签名")?)
        .map_err(|error| format!("更新签名格式无效：{error}"))?;
    public_key.verify(data, &signature, true).map_err(|error| {
        format!("更新包签名校验失败（文件可能被篡改或没下载完整）：{error}")
    })
}

fn decode_base64(value: &str, what: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|error| format!("{what}编码无效：{error}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{what}不是合法的 UTF-8 文本"))
}

/// 把 `reqwest` 的错误连同底层原因一起拼出来。
///
/// 只打 `error.to_string()` 时永远只有一句 "error sending request"，用户和日志都看不出
/// 到底是 DNS、代理还是 TLS 挂了；这里顺着 `source()` 一路展开。
fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        text.push_str("：");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    text
}

fn build_client(proxy: Option<&str>, direct: bool) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .user_agent(DEFAULT_HTTP_USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT);
    if direct {
        // 镜像站强制直连：系统代理会把镜像流量带去海外节点，实测能把
        // 6.7 MB/s 拖到 25 秒零字节。镜像本来就该绕开代理。
        // 连接超时也收短：镜像被墙时要快点落到下一个候选，而不是干等 15 秒。
        builder = builder.no_proxy().connect_timeout(MIRROR_CONNECT_TIMEOUT);
    } else if let Some(proxy) = proxy.map(str::trim).filter(|value| !value.is_empty()) {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy).map_err(|error| format!("更新代理地址无效：{error}"))?,
        );
    }
    builder
        .build()
        .map_err(|error| format!("初始化下载器失败：{error}"))
}

/// 是否是我们内置的镜像地址。
///
/// 只有**已知镜像**才强制直连：自定义更新源（第三方自建 CDN）在必须走代理的
/// 网络里还得靠代理，不能因为「不是 github.com」就把它绕过去。
pub fn is_mirror_url(url: &str) -> bool {
    MIRROR_PREFIXES.iter().any(|prefix| {
        url.strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
    })
}

struct Probe {
    total: Option<u64>,
    ranged: bool,
}

/// 等到取消标志置位为止：把「本来不可打断的等待」（建链、等响应头）变成可打断的。
///
/// 节拍与 [D7] 的分块轮询同为 `CANCEL_POLL`，所以取消的响应上界仍然是 500ms 级。
async fn wait_until_cancelled(control: &DownloadControl) {
    while !control.is_cancelled() {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

/// 探测：只看响应头，拿总长度与是否支持 Range。
///
/// 建链最长要吃满 `MIRROR_CONNECT_TIMEOUT`（6 秒），这段等待必须能被取消打断，
/// 否则用户点了「取消下载」还得干等建链超时，UI 一直卡在「正在取消…」。
async fn probe(
    client: &reqwest::Client,
    url: &str,
    control: &DownloadControl,
) -> Result<Probe, String> {
    control.check()?;
    let request = client
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .timeout(PROBE_TIMEOUT)
        .send();
    let response = tokio::select! {
        result = request => result.map_err(|error| format!("连接失败：{}", describe(&error)))?,
        () = wait_until_cancelled(control) => return Err(CANCELLED_ERROR.to_string()),
    };
    let status = response.status();
    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let total = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range);
        return Ok(Probe {
            total,
            ranged: true,
        });
    }
    if status.is_success() {
        // 不支持 Range：整包单连接拉，总量取 Content-Length。
        return Ok(Probe {
            total: response.content_length(),
            ranged: false,
        });
    }
    Err(format!("返回 HTTP {}", status.as_u16()))
}

/// 从单个候选地址下载整包字节。分段是否启用由探测结果决定。
///
/// `control` 由宿主用 `Arc` 持有（取消命令也要拿它），这里每个外部动作前都会过一遍检查点。
pub async fn fetch(
    url: &str,
    proxy: Option<&str>,
    control: &Arc<DownloadControl>,
    progress: Progress,
) -> Result<Vec<u8>, String> {
    control.check()?;
    // 进度归零不变量（[D5]）：每个候选都从 0 重新累计，换源后百分比必须能回落。
    control.reset_progress();
    let client = build_client(proxy, is_mirror_url(url))?;
    let probe = probe(&client, url, control).await?;
    let total = probe.total;
    let bytes = match total {
        Some(total) if probe.ranged && total >= MIN_SEGMENT_BYTES => {
            match download_segmented(&client, url, total, control, &progress).await {
                Ok(bytes) => bytes,
                // 取消 / 停滞是控制类失败：再退回单连接等于把用户刚点的取消又拖回去。
                Err(error) if is_control_error(&error) => return Err(error),
                Err(error) => {
                    // 探测说支持 Range、实际分段却失败了（镜像前后不一致）：退回单连接再试一次，
                    // 别因为一次抖动把整条通道判死。
                    eprintln!("[更新] 分段下载失败（{error}），改用单连接重试");
                    control.reset_progress();
                    download_stream(&client, url, Some(total), control, &progress).await?
                }
            }
        }
        _ => download_stream(&client, url, total, control, &progress).await?,
    };
    // 收尾再查一次：用户可能在最后一块之后才点的取消。
    control.check()?;
    Ok(bytes)
}

async fn download_segmented(
    client: &reqwest::Client,
    url: &str,
    total: u64,
    control: &Arc<DownloadControl>,
    progress: &Progress,
) -> Result<Vec<u8>, String> {
    let mut tasks = tokio::task::JoinSet::new();
    for (index, (start, end)) in plan_segments(total, SEGMENT_CONNECTIONS).into_iter().enumerate()
    {
        let client = client.clone();
        let url = url.to_string();
        let progress = Arc::clone(progress);
        // 分段任务必须看见同一份控制块：取消 / 停滞要立刻砸掉全部 N 条连接。
        let control = Arc::clone(control);
        tasks.spawn(async move {
            download_range(&client, &url, (start, end), total, &control, &progress)
                .await
                .map(|bytes| (index, bytes))
        });
    }

    let mut parts: Vec<(usize, Vec<u8>)> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (index, bytes) = match joined {
            Ok(Ok(part)) => part,
            Ok(Err(error)) if is_control_error(&error) => {
                // 一段被取消 / 判涓流就立刻放弃其余连接，别让 3 条死连接继续拖着。
                tasks.abort_all();
                return Err(error);
            }
            Ok(Err(error)) => return Err(error),
            Err(error) => return Err(format!("分段下载任务异常退出：{error}")),
        };
        parts.push((index, bytes));
    }
    parts.sort_by_key(|(index, _)| *index);

    let mut out = Vec::with_capacity(total as usize);
    for (_, part) in parts {
        out.extend_from_slice(&part);
    }
    if out.len() as u64 != total {
        return Err(format!(
            "下载不完整：期望 {total} 字节，实得 {} 字节",
            out.len()
        ));
    }
    Ok(out)
}

async fn download_range(
    client: &reqwest::Client,
    url: &str,
    (start, end): (u64, u64),
    total: u64,
    control: &DownloadControl,
    progress: &Progress,
) -> Result<Vec<u8>, String> {
    let mut last_error = String::new();
    for attempt in 0..=SEGMENT_RETRIES {
        let mut counted = 0u64;
        match stream_range(client, url, start, end, total, control, &mut counted, progress).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                // 这一段已计入的字节要退回，否则重试后进度会虚高。
                control.release_bytes(counted);
                // 取消 / 停滞不重试：用户已经点了取消，或这条通道已被判涓流，重试只是拖延换源。
                if is_control_error(&error) {
                    return Err(error);
                }
                last_error = error;
            }
        }
        if attempt < SEGMENT_RETRIES {
            tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
        }
    }
    Err(format!("分片 {start}-{end} 下载失败：{last_error}"))
}

#[allow(clippy::too_many_arguments)]
async fn stream_range(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    total: u64,
    control: &DownloadControl,
    counted: &mut u64,
    progress: &Progress,
) -> Result<Vec<u8>, String> {
    control.check()?;
    let response = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|error| format!("请求失败：{}", describe(&error)))?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format!(
            "不支持分段下载（HTTP {}）",
            response.status().as_u16()
        ));
    }
    let expected = end - start + 1;
    let mut buffer = Vec::with_capacity(expected as usize);
    for_each_chunk(response, control, |chunk| {
        buffer.extend_from_slice(&chunk[..]);
        *counted += chunk.len() as u64;
        control.record_bytes(chunk.len() as u64);
        progress(control.downloaded(), Some(total));
    })
    .await?;
    if buffer.len() as u64 != expected {
        return Err(format!(
            "分段长度不符：期望 {expected} 字节，实得 {} 字节",
            buffer.len()
        ));
    }
    Ok(buffer)
}

async fn download_stream(
    client: &reqwest::Client,
    url: &str,
    total: Option<u64>,
    control: &DownloadControl,
    progress: &Progress,
) -> Result<Vec<u8>, String> {
    control.check()?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("请求失败：{}", describe(&error)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("返回 HTTP {}", status.as_u16()));
    }
    let total = total.or_else(|| response.content_length());
    let mut buffer = Vec::with_capacity(total.unwrap_or_default() as usize);
    for_each_chunk(response, control, |chunk| {
        buffer.extend_from_slice(&chunk[..]);
        control.record_bytes(chunk.len() as u64);
        progress(control.downloaded(), total);
    })
    .await?;
    if let Some(total) = total {
        if buffer.len() as u64 != total {
            return Err(format!(
                "下载不完整：期望 {total} 字节，实得 {} 字节",
                buffer.len()
            ));
        }
    }
    Ok(buffer)
}

/// 逐块读取响应体，每 500ms 一个检查点。
///
/// 为什么不再直接 `timeout(IDLE_TIMEOUT, chunk())`：那样在整个等待期间都是不可打断的，
/// 用户点「取消」得等满 20 秒才生效。改成 `timeout(CANCEL_POLL, chunk())` 轮询后：
/// 每轮先看取消 / 停滞标志（响应上界 500ms），拿到块就重置空闲计时，
/// 连续无字节累计到 `IDLE_TIMEOUT` 才报空闲超时 —— 语义与原来一致，只是可打断。
async fn for_each_chunk(
    mut response: reqwest::Response,
    control: &DownloadControl,
    mut on_chunk: impl FnMut(&[u8]),
) -> Result<(), String> {
    let mut idle_deadline = Instant::now() + IDLE_TIMEOUT;
    loop {
        control.check()?;
        match tokio::time::timeout(CANCEL_POLL, response.chunk()).await {
            Err(_) if Instant::now() >= idle_deadline => {
                return Err("连接空闲超时（20 秒没有收到数据）".to_string());
            }
            // 只是这一轮没等到数据：回到循环顶部重新看取消标志，不重置空闲计时。
            Err(_) => {}
            Ok(Err(error)) => return Err(format!("传输中断：{}", describe(&error))),
            Ok(Ok(None)) => return Ok(()),
            Ok(Ok(Some(chunk))) => {
                idle_deadline = Instant::now() + IDLE_TIMEOUT;
                on_chunk(&chunk);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_mirrors_bypass_the_proxy() {
        assert!(is_mirror_url(
            "https://gh-proxy.com/https://github.com/o/r/releases/download/a.exe"
        ));
        assert!(is_mirror_url(
            "https://ghproxy.net/https://github.com/o/r/releases/download/a.exe"
        ));
        // GitHub 直连与第三方自建源都保留系统/显式代理。
        assert!(!is_mirror_url(
            "https://github.com/o/r/releases/download/a.exe"
        ));
        assert!(!is_mirror_url("https://cdn.example.com/wb-switch/a.exe"));
    }

    #[test]
    fn build_client_accepts_empty_and_direct_modes() {
        assert!(build_client(None, false).is_ok());
        assert!(build_client(Some("http://127.0.0.1:7897"), false).is_ok());
        // 直连模式会忽略代理参数：镜像永远不该被系统代理接管。
        assert!(build_client(Some("http://127.0.0.1:7897"), true).is_ok());
    }

    #[test]
    fn mirror_url_trims_trailing_slash_and_keeps_original_on_empty_prefix() {
        assert_eq!(
            mirror_url("https://github.com/o/r/releases/download/a.exe", "https://gh-proxy.com/"),
            "https://gh-proxy.com/https://github.com/o/r/releases/download/a.exe"
        );
        assert_eq!(
            mirror_url("https://github.com/o/r/a.exe", "  "),
            "https://github.com/o/r/a.exe"
        );
    }

    #[test]
    fn download_candidates_put_mirrors_first_and_direct_last() {
        let url = "https://github.com/o/r/releases/download/v1/a.exe";
        let candidates = download_candidates(url);
        assert_eq!(candidates.len(), MIRROR_PREFIXES.len() + 1);
        assert!(candidates[0].starts_with(MIRROR_PREFIXES[0]));
        assert_eq!(candidates.last().map(String::as_str), Some(url));
    }

    #[test]
    fn download_candidates_leave_third_party_sources_untouched() {
        let url = "https://example.com/updates/a.exe";
        assert_eq!(download_candidates(url), vec![url.to_string()]);
    }

    #[test]
    fn plan_segments_covers_every_byte_exactly_once() {
        let total = 8_815_103u64;
        let segments = plan_segments(total, SEGMENT_CONNECTIONS);
        assert_eq!(segments.len(), SEGMENT_CONNECTIONS);
        assert_eq!(segments[0].0, 0);
        assert_eq!(segments[segments.len() - 1].1, total - 1);
        for pair in segments.windows(2) {
            assert_eq!(pair[0].1 + 1, pair[1].0, "分片之间不能有缝隙或重叠");
        }
        let covered: u64 = segments.iter().map(|(start, end)| end - start + 1).sum();
        assert_eq!(covered, total);
    }

    #[test]
    fn plan_segments_handles_tiny_and_empty_inputs() {
        assert!(plan_segments(0, SEGMENT_CONNECTIONS).is_empty());
        assert!(plan_segments(1024, 0).is_empty());
        assert_eq!(plan_segments(3, SEGMENT_CONNECTIONS), vec![(0, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn parse_content_range_reads_total_length() {
        assert_eq!(parse_content_range("bytes 0-0/8815103"), Some(8_815_103));
        assert_eq!(parse_content_range("bytes 0-0/*"), None);
        assert_eq!(parse_content_range("garbage"), None);
    }

    /// 真签名必须验得过，动一个字节就必须失败 —— 这是「镜像可以不可信，但包不能」的底线。
    const FIXTURE_PAYLOAD: &str = "wb-switch update signature fixture\n";
    const FIXTURE_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDM2ODhEOUNEMkMzMTJCRjIKUldUeUt6RXN6ZG1JTmtjUU11aGxKZG9hdnQvTVpLNWdvd09XVk1TSmpvRjdqcllLZHpDOWZMUloK";
    const FIXTURE_SIG: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVUeUt6RXN6ZG1JTnNWTjN1ZGRCcUJubG9pcW5DOE5xVVQzdElhUjRSZmhCcG1JaytlVmZTcXpLc05PNWROZGFJTjBwOWhXNW9aWnYrMkl2bWV3cVloKzFJblBxY2NSS2c0PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkzNzkyMzU1CWZpbGU6cGF5bG9hZC5iaW4KdDlEYlgrS21GQjFLakMvUTJINy9oU0RaRm1KelRXQUdFTGMxUzdYdGFCSlU1M2t2aWszWm5qUTVMKy9HQW4xVWpvYlFhdktxTU9yZzNSZWJaV2FWQlE9PQo=";

    #[test]
    fn verify_signature_accepts_matching_payload() {
        assert!(verify_signature(FIXTURE_PAYLOAD.as_bytes(), FIXTURE_SIG, FIXTURE_PUBKEY).is_ok());
    }

    #[test]
    fn verify_signature_rejects_tampered_payload() {
        let tampered = FIXTURE_PAYLOAD.replace("fixture", "tamper!");
        let error = verify_signature(tampered.as_bytes(), FIXTURE_SIG, FIXTURE_PUBKEY)
            .expect_err("改过的内容必须验签失败");
        assert!(error.contains("签名校验失败"), "{error}");
    }

    #[test]
    fn verify_signature_reports_broken_encoding() {
        let error = verify_signature(b"payload", "not-base64!!", FIXTURE_PUBKEY)
            .expect_err("非法 base64 必须报错");
        assert!(error.contains("编码无效"), "{error}");
    }

    // ---------------------------------------------------------------------
    // 涓流判据（[D1]）：16.7 KB/s 的限速镜像必须判死，300 KB/s 的正常镜像不许误杀
    // ---------------------------------------------------------------------

    /// 实测限速源：15 秒窗口里恒定 16 KB/s —— 就是「卡在 85% 再也不动」的那种通道。
    #[test]
    fn 窗口判据_每秒十六kb的限速通道判涓流() {
        let window = STALL_WINDOW;
        let bytes = 16 * 1024 * window.as_secs();
        assert!(throughput_is_stalled(window, bytes, Some(9_072_340)));
    }

    /// 实测正常源：15 秒窗口里恒定 300 KB/s —— 误判会把好通道也换掉，比卡住更糟。
    #[test]
    fn 窗口判据_每秒三百kb的正常通道不判涓流() {
        let window = STALL_WINDOW;
        let bytes = 300 * 1024 * window.as_secs();
        assert!(!throughput_is_stalled(window, bytes, Some(9_072_340)));
    }

    /// 边界③：总长未知时不判 —— 无法排除「正常慢速的小文件」，只受候选预算约束。
    #[test]
    fn 窗口判据_总长未知时不判涓流() {
        assert!(!throughput_is_stalled(STALL_WINDOW, 0, None));
        assert!(!throughput_is_stalled(Duration::from_secs(60), 0, None));
    }

    /// 边界①：已下满（100%）之后速度归零是正常收尾，不能判涓流。
    #[test]
    fn 窗口判据_已下满百分之百时不判涓流() {
        let total = 9_072_340;
        assert!(!throughput_is_stalled(STALL_WINDOW, total, Some(total)));
        assert!(!throughput_is_stalled(STALL_WINDOW, total + 1, Some(total)));
    }

    /// 边界②：前 5 秒是建链与首包，一律不判；跨过预热期立刻按地板判。
    #[test]
    fn 窗口判据_预热五秒内不判_跨过预热立刻判() {
        let total = Some(9_072_340);
        assert!(!throughput_is_stalled(
            STALL_WARMUP - Duration::from_millis(1),
            0,
            total
        ));
        assert!(throughput_is_stalled(STALL_WARMUP, 0, total));
    }

    /// 检查点（[D7]）：取消 / 停滞置位后必须立刻返回对应文案，且能被精确识别为控制类失败。
    #[test]
    fn 检查点_取消与停滞置位后返回对应文案() {
        let control = DownloadControl::new();
        assert!(control.check().is_ok(), "刚创建的下载不该被拦");

        control.stall("窗口吞吐 16 KB/s 低于 80 KB/s 地板");
        assert!(control.is_stalled());
        let error = control.check().expect_err("停滞必须拦住下载");
        assert_eq!(error, STALLED_ERROR);
        assert!(is_control_error(&error), "停滞属于控制类失败，不许重试");

        control.cancel();
        assert!(control.is_cancelled());
        let error = control.check().expect_err("取消优先级最高");
        assert_eq!(error, CANCELLED_ERROR);
        assert!(is_control_error(&error), "取消属于控制类失败，不许重试");
    }

    /// 取消幂等，且不能抹掉累计字节（宿主还要拿它算进度）；换源前必须能归零。
    #[test]
    fn 控制块_取消幂等且累计字节可回退可归零() {
        let control = DownloadControl::new();
        control.record_bytes(1024);
        control.record_bytes(1024);
        control.release_bytes(24);
        assert_eq!(control.downloaded(), 2024);
        control.cancel();
        control.cancel();
        assert!(control.is_cancelled());
        assert_eq!(control.downloaded(), 2024, "取消不该清空已下载字节");
        control.reset_progress();
        assert_eq!(control.downloaded(), 0, "换源前进度必须能归零");
    }

    /// 已取消的控制块：在 `send()` 之前就被拦住，连探测请求都不该发出去。
    #[tokio::test]
    async fn 已取消的控制块_发起下载立即返回已取消() {
        let control = Arc::new(DownloadControl::new());
        control.cancel();
        let progress: Progress = Arc::new(|_, _| {});
        // 指向必然连不通的地址：真发请求会等建连超时，只有前置检查才能立刻失败。
        let error = fetch("http://127.0.0.1:1/a.exe", None, &control, progress)
            .await
            .expect_err("取消后不该继续下载");
        assert_eq!(error, CANCELLED_ERROR);
    }

    // ---------------------------------------------------------------------
    // 分段下载端到端验证：本地起一个只认 Range 的迷你 HTTP 服务
    // ---------------------------------------------------------------------

    /// 迷你 HTTP 服务：只会「按 Range 回 206」和「整包回 200」两件事。
    ///
    /// 用它验证真实网络路径（探测 → 分段 → 拼装 → 长度校验），不依赖外网和 TLS，
    /// 沙箱里也能稳定跑。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RangeMode {
        /// 老实支持 Range。
        Honest,
        /// 完全无视 Range，永远整包 200。
        Ignore,
        /// 只在探测请求（bytes=0-0）上回 206，其余分段请求回 200 —— 模拟「说支持、
        /// 实际不支持」的镜像。
        ProbeOnly,
        /// 老实回 206，但只吐一小段就挂住不关 —— 模拟「涓流镜像中途断流」，
        /// 用来验证取消 / 停滞检查点能真的打断进行中的读取。
        Stall,
    }

    struct RangeServer {
        address: String,
        stop: Arc<std::sync::atomic::AtomicBool>,
        range_requests: Arc<AtomicU64>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl RangeServer {
        fn start(body: Vec<u8>, mode: RangeMode) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定端口失败");
            listener.set_nonblocking(true).expect("设置非阻塞失败");
            let address = format!("http://{}", listener.local_addr().expect("读取地址失败"));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let range_requests = Arc::new(AtomicU64::new(0));
            let body = Arc::new(body);
            let handle = {
                let stop = Arc::clone(&stop);
                let range_requests = Arc::clone(&range_requests);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                let body = Arc::clone(&body);
                                let range_requests = Arc::clone(&range_requests);
                                // 每个连接一个线程：分段是并发的，串行服务会把彼此堵住。
                                std::thread::spawn(move || {
                                    respond(&mut stream, &body, mode, &range_requests)
                                });
                            }
                            Err(error)
                                if error.kind() == std::io::ErrorKind::WouldBlock =>
                            {
                                std::thread::sleep(Duration::from_millis(2));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };
            Self {
                address,
                stop,
                range_requests,
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!("{}/package.exe", self.address)
        }

        fn range_requests(&self) -> u64 {
            self.range_requests.load(Ordering::Relaxed)
        }
    }

    impl Drop for RangeServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn respond(
        stream: &mut std::net::TcpStream,
        body: &[u8],
        mode: RangeMode,
        range_requests: &AtomicU64,
    ) {
        use std::io::Write;

        let request = read_request_head(stream);
        if mode == RangeMode::Stall {
            respond_stalled(stream, body, &request, range_requests);
            return;
        }
        let range = match mode {
            RangeMode::Ignore => None,
            RangeMode::Honest | RangeMode::Stall => parse_request_range(&request),
            RangeMode::ProbeOnly => parse_request_range(&request)
                .filter(|(start, end)| *start == 0 && *end == 0),
        };
        let response = match range {
            Some((start, end)) => {
                range_requests.fetch_add(1, Ordering::Relaxed);
                let start = start as usize;
                let end = (end as usize).min(body.len().saturating_sub(1));
                let slice = &body[start..=end];
                let mut head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    slice.len(),
                    start,
                    end,
                    body.len()
                )
                .into_bytes();
                head.extend_from_slice(slice);
                head
            }
            None => {
                let mut head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                head.extend_from_slice(body);
                head
            }
        };
        let _ = stream.write_all(&response);
        let _ = stream.flush();
    }

    /// Stall 模式的服务端：回真实的 206 头 + 第一块 4 KiB，然后挂住不关连接。
    ///
    /// 头部按整段长度声明 `Content-Length`，客户端会一直等剩下的字节 —— 正是
    /// 「涓流镜像中途断流」的现场，检查点必须能从这种等待里把控制权抢回来。
    fn respond_stalled(
        stream: &mut std::net::TcpStream,
        body: &[u8],
        request: &str,
        range_requests: &AtomicU64,
    ) {
        use std::io::Write;

        let (start, end) = parse_request_range(request)
            .map(|(start, end)| {
                (
                    start as usize,
                    (end as usize).min(body.len().saturating_sub(1)),
                )
            })
            .unwrap_or((0, body.len().saturating_sub(1)));
        range_requests.fetch_add(1, Ordering::Relaxed);
        let slice = &body[start..=end];
        let head = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            slice.len(),
            body.len()
        )
        .into_bytes();
        let _ = stream.write_all(&head);
        let _ = stream.write_all(&slice[..slice.len().min(4096)]);
        let _ = stream.flush();
        // 挂住：让客户端的 `chunk()` 一直等下去，用例必须在 2 秒内把控制权抢回来。
        std::thread::sleep(Duration::from_secs(5));
    }

    /// 读到空行为止：请求头可能被拆成多个 TCP 段，只读一次会漏掉 `Range`。
    fn read_request_head(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;

        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buffer = Vec::with_capacity(1024);
        let mut chunk = [0u8; 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    buffer.extend_from_slice(&chunk[..read]);
                    if buffer.windows(4).any(|window| window == b"\r\n\r\n") || buffer.len() >= 8192 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buffer).to_string()
    }

    fn parse_request_range(request: &str) -> Option<(u64, u64)> {
        let line = request
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("range:"))?;
        let spec = line.split_once(':')?.1.trim().strip_prefix("bytes=")?;
        let (start, end) = spec.split_once('-')?;
        Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
    }

    #[tokio::test]
    async fn fetch_uses_parallel_ranges_and_reassembles_exact_bytes() {
        let body: Vec<u8> = (0..3 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        let server = RangeServer::start(body.clone(), RangeMode::Honest);
        let progress: Progress = Arc::new(|_, _| {});
        let control = Arc::new(DownloadControl::new());
        let downloaded = fetch(&server.url(), None, &control, progress)
            .await
            .expect("分段下载失败");
        assert_eq!(downloaded, body, "拼装结果必须与原始字节完全一致");
        assert!(
            server.range_requests() >= SEGMENT_CONNECTIONS as u64,
            "应当真的并发走了多段，实际只有 {} 段",
            server.range_requests()
        );
    }

    #[tokio::test]
    async fn fetch_falls_back_to_single_stream_when_server_ignores_range() {
        let body: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|index| (index % 97) as u8)
            .collect();
        let server = RangeServer::start(body.clone(), RangeMode::Ignore);
        let progress: Progress = Arc::new(|_, _| {});
        let control = Arc::new(DownloadControl::new());
        let downloaded = fetch(&server.url(), None, &control, progress)
            .await
            .expect("整包下载失败");
        assert_eq!(downloaded, body);
        assert_eq!(server.range_requests(), 0, "服务端不支持 Range 时不该尝试分段");
    }

    /// 镜像「探测时回 206、真分段时回 200」是常见坑：不能因此整条通道判死，
    /// 应该退回单连接把包拉完。
    #[tokio::test]
    async fn fetch_falls_back_to_single_stream_when_ranges_lie() {
        let body: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|index| (index % 131) as u8)
            .collect();
        let server = RangeServer::start(body.clone(), RangeMode::ProbeOnly);
        let progress: Progress = Arc::new(|_, _| {});
        let control = Arc::new(DownloadControl::new());
        let downloaded = fetch(&server.url(), None, &control, progress)
            .await
            .expect("分段失败后应该退回单连接");
        assert_eq!(downloaded, body);
    }

    /// 真机自检（默认不跑）：镜像下载必须无视系统代理。
    ///
    /// ```text
    /// cargo test -p wb-switch-core --lib live_mirror_ignores_system_proxy -- --ignored --test-threads=1
    /// ```
    ///
    /// 它会把进程代理指到一个死端口，所以必须单线程跑——别和别的用例并行。
    /// 通过即证明 `.no_proxy()` 生效：系统代理开着（或被墙）也不会拖垮镜像下载。
    #[tokio::test]
    #[ignore]
    async fn live_mirror_ignores_system_proxy() {
        const KEYS: [&str; 6] = [
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "ALL_PROXY",
            "https_proxy",
            "http_proxy",
            "all_proxy",
        ];
        let url = "https://gh-proxy.com/https://github.com/Binbin-miao/workbuddy-switch-binbinbin/releases/latest/download/latest.json";
        let previous: Vec<(&str, Option<std::ffi::OsString>)> =
            KEYS.iter().map(|key| (*key, std::env::var_os(key))).collect();
        for key in KEYS {
            std::env::set_var(key, "http://127.0.0.1:1");
        }
        let progress: Progress = Arc::new(|_, _| {});
        let control = Arc::new(DownloadControl::new());
        let result = fetch(url, None, &control, progress).await;
        for (key, value) in previous {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        let body = result.expect("镜像应当绕开系统代理直连成功");
        assert!(
            String::from_utf8_lossy(&body).contains("version"),
            "下载内容看起来不是更新清单"
        );
    }

    /// 空输入不该 panic、也不该造出候选；单元素直接原样返回（连测速都省掉）。
    #[tokio::test]
    async fn rank_candidates_handles_empty_and_single_input_without_network() {
        assert!(rank_candidates(&[], None).await.is_empty());
        let single = vec!["https://example.com/updates/a.exe".to_string()];
        assert_eq!(rank_candidates(&single, None).await, single);
    }

    /// 两条都不会连通的候选：必须原样保留（慢/死只是排序靠后，不能把兜底删掉）。
    #[tokio::test]
    async fn rank_candidates_keeps_every_candidate_even_when_all_probes_fail() {
        let candidates = vec![
            "http://127.0.0.1:1/a.exe".to_string(),
            "http://127.0.0.1:2/b.exe".to_string(),
        ];
        let ranked = rank_candidates(&candidates, None).await;
        assert_eq!(ranked.len(), 2);
        let mut sorted_input = candidates.clone();
        let mut sorted_output = ranked.clone();
        sorted_input.sort();
        sorted_output.sort();
        assert_eq!(sorted_input, sorted_output);
    }

    /// 端到端（[D4] 响应上界）：读到一半点取消，必须在 500ms 级返回「已取消」，
    /// 而不是等这条连接自己读完或空闲超时。断言顺带证明控制类失败不重试分片。
    #[tokio::test]
    async fn 取消_流式中途置位后两秒内返回已取消() {
        let body: Vec<u8> = (0..3 * 1024 * 1024).map(|index| (index % 251) as u8).collect();
        let server = RangeServer::start(body, RangeMode::Stall);
        let control = Arc::new(DownloadControl::new());
        let progress: Progress = Arc::new(|_, _| {});
        let url = server.url();
        let task_control = Arc::clone(&control);
        let started = Instant::now();
        let download = tokio::spawn(async move { fetch(&url, None, &task_control, progress).await });

        // 等到探测 + 4 条分片都建好连接（说明真的在流式读），再点取消。
        while server.range_requests() < 5 && started.elapsed() < Duration::from_secs(3) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(server.range_requests() >= 5, "分片连接没起来，用例前提不成立");

        control.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), download)
            .await
            .expect("取消后必须立刻返回，不能等连接自己结束")
            .expect("下载任务不该 panic");
        assert_eq!(result.expect_err("取消必须让下载失败"), CANCELLED_ERROR);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "取消响应超上界：{:?}",
            started.elapsed()
        );
        assert_eq!(server.range_requests(), 5, "取消属于控制类失败，不许重试分片");
    }

    /// 端到端（[D1] 判定结果）：停滞置位后必须返回「速度过低」且**不重试**。
    ///
    /// 重试会把换源再拖后 2×300ms 起，还可能让后面本来能用的候选永远轮不到。
    #[tokio::test]
    async fn 停滞_置位后返回速度过低且不重试() {
        let body: Vec<u8> = (0..3 * 1024 * 1024).map(|index| (index % 131) as u8).collect();
        let server = RangeServer::start(body, RangeMode::Stall);
        let control = Arc::new(DownloadControl::new());
        let progress: Progress = Arc::new(|_, _| {});
        let url = server.url();
        let task_control = Arc::clone(&control);
        let started = Instant::now();
        let download = tokio::spawn(async move { fetch(&url, None, &task_control, progress).await });

        while server.range_requests() < 5 && started.elapsed() < Duration::from_secs(3) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(server.range_requests() >= 5, "分片连接没起来，用例前提不成立");

        control.stall("窗口吞吐 16 KB/s 低于 80 KB/s 地板");
        let result = tokio::time::timeout(Duration::from_secs(3), download)
            .await
            .expect("停滞判定后必须立刻放弃这条通道")
            .expect("下载任务不该 panic");
        assert_eq!(result.expect_err("停滞必须让下载失败"), STALLED_ERROR);
        assert_eq!(
            server.range_requests(),
            5,
            "控制类失败不许重试：探测 1 次 + 分片各 1 次"
        );
    }
}
