use anyhow::{anyhow, Result};
use once_cell::sync::{Lazy, OnceCell};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::ops::{Deref, DerefMut};
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use sysinfo::{CpuExt, DiskExt, System, SystemExt};
use tokio::runtime::Runtime;
use tokio::sync::broadcast::Sender;
use tracing::{error, info};
mod clean;
pub mod entity;
pub mod push;
mod websocket;

pub use entity::*;

// 使用泛型 T 的包装类型
pub struct Global<T>(OnceCell<T>);

// 为泛型实现 Deref trait
impl<T> Deref for Global<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.0.get_unchecked() }
    }
}

// 为泛型实现通用方法
impl<T> Global<T> {
    // 创建新实例
    pub const fn new() -> Self {
        Self(OnceCell::new())
    }

    // 初始化方法
    pub fn init(&self, value: T) -> Result<(), T> {
        self.0.set(value)
    }

    // 安全获取值的方法
    pub fn get(&self) -> Option<&T> {
        self.0.get()
    }

    // 检查是否已初始化
    pub fn is_initialized(&self) -> bool {
        self.0.get().is_some()
    }
}

/// 爬虫统计
pub(crate) static SPIDER_STATS: Lazy<RequestStats> = Lazy::new(|| RequestStats::new());

pub(crate) static SPIDER_STATS_PUSH: Global<Sender<String>> = Global::new();

pub(crate) static GET_HOSTS: Global<Box<dyn Fn() -> Result<Vec<String>> + Send + Sync>> =
    Global::new();

pub(crate) static GET_BASE: Global<Box<dyn Fn() -> StatsBase + Send + Sync>> = Global::new();

pub(crate) static GLOBAL_RUNTIME: Lazy<Runtime> = Lazy::new(|| get_new_rn(3, "util"));

fn get_new_rn(num: usize, th_name: &str) -> Runtime {
    let rn = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num)
        .thread_name(th_name)
        .enable_all()
        .build()
        .unwrap();
    rn
}

fn get_now_millis() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
// 初始化爬虫推送
pub fn init_spider_vars(
    config: RequestStatsConfig,
    // base: StatsBase,
    get_base_call: Box<dyn Fn() -> StatsBase + Send + Sync>,
    get_host_call: Box<dyn Fn() -> Result<Vec<String>> + Send + Sync>,

    // 清理过期文件目录; 过期时间；这个是根据文件创建时间来判断的
    clean_paths: Option<(Vec<String>, Duration)>,
) -> Result<()> {
    let s = push::load_broadcast_chan(config.target.clone());

    SPIDER_STATS_PUSH
        .init(s)
        .map_err(|err| anyhow!("{:?}", err))?;

    GET_HOSTS
        .init(get_host_call)
        .map_err(|e| anyhow!("设置 get host call 失败"))?;

    GET_BASE
        .init(get_base_call)
        .map_err(|e| anyhow!("设置 get base call 失败"))?;

    // 开启线程；定时去发送任务信息
    thread::spawn(move || loop {
        thread::sleep(config.reporting_cycle);

        let host = match GET_HOSTS() {
            Ok(s) => Some((s, config.host_test_port)),
            Err(err) => {
                error!("获取 hosts 数据失败：{}", err);
                None
            }
        };

        let base = GET_BASE();

        send_stats(&base, host);

        if let Some((clean_paths, max_ts)) = &clean_paths {
            for p in clean_paths {
                if let Err(err) = clean::clean_old_files(p, *max_ts) {
                    error!("删除 {p} 目录下的过期文件失败 : {}", err);
                }
            }
        }
    });

    Ok(())
}

// 更新爬虫统计状态
pub fn update_stats(
    request_time: i64,
    response_time: i64,
    status_code: u16,
    result: RequestResult, // 使用枚举表示请求结果
) {
    SPIDER_STATS.update_stats(request_time, response_time, status_code, result)
}

// 更新爬虫统计状态
pub fn send_stats(
    base: &StatsBase,

    // 用于测试 hosts 的延迟
    host_info: Option<(Vec<String>, u16)>,
) {
    let stats = SPIDER_STATS.to_stats_and_reset(base, host_info);

    let msg = serde_json::to_string(&stats).unwrap();

    if let Err(err) = SPIDER_STATS_PUSH.send(msg) {
        info!("发送统计信息失败：{}", err);
    }

    let msg = serde_json::to_string_pretty(&stats).unwrap();
    info!("发送统计信息: {}", msg);
}

pub struct RequestStats {
    inner: Mutex<InnerStats>,
}

impl RequestStats {
    /// 创建一个新的统计实例，并记录初始化时间和开始时间
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(InnerStats::new()),
        }
    }

    /// 更新统计信息的方法
    pub fn update_stats(
        &self,
        request_time: i64,
        response_time: i64,
        status_code: u16,
        result: RequestResult, // 使用枚举表示请求结果
    ) {
        self.inner
            .lock()
            .update_stats(request_time, response_time, status_code, result);
    }

    /// 将当前统计数据拼装到 `Stats` 结构体中，并清空当前统计数据
    /// 统计的时候需要传入 hosts + port 信息
    pub fn to_stats_and_reset<'a>(
        &self,
        base: &'a StatsBase,

        // 用于测试 hosts 的延迟
        host_info: Option<(Vec<String>, u16)>,
    ) -> Stats<'a> {
        let mut host_ping = HashMap::new();

        if let Some((hosts, port)) = host_info {
            let timeout = Duration::from_secs(3);

            for host in hosts {
                let connet_ts = match run_test_tcp(&host, port, timeout) {
                    Ok(d) => d,
                    Err(_) => timeout.as_micros() as u64,
                };

                // 0.6ms
                // 微秒转成毫秒
                let ms = connet_ts as f64 / 1000.0;
                host_ping.insert(host, ms);
            }
        }

        let mut data = self.inner.lock();
        let mut d = data.to_stats_and_reset(base);
        data.reset();

        d.hosts_ping_delay = host_ping;

        d
    }
}

struct InnerStats {
    // 对象初始化时间（毫秒级时间戳）
    pub init_time: i64,
    // 当前统计周期的开始时间（毫秒级时间戳）
    pub start_time: i64,

    pub base: InnerStatsVal,
}

impl Deref for InnerStats {
    type Target = InnerStatsVal;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl DerefMut for InnerStats {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
}

#[derive(Default)]
struct InnerStatsVal {
    // 总请求数
    pub total_requests: i64,
    // 成功请求数
    pub successful_requests: i64,
    // 命中缓存的次数；在请求成功的情况下才统计
    pub cache_hit: i64,
    // 解析失败次数
    pub parse_errors: i64,
    // 超时错误次数
    pub timeout_errors: i64,
    // 连接失败次数
    pub connection_errors: i64,
    pub status_code_error: i64,
    // HTTP 状态码统计（键为状态码，值为出现次数）
    pub http_status_codes: HashMap<u16, i64>,
    // 总请求延迟（毫秒）
    pub total_latency: i64,
}

impl InnerStats {
    /// 创建一个新的统计实例，并记录初始化时间和开始时间
    fn new() -> Self {
        let current_time = get_now_millis();
        Self {
            init_time: current_time,
            start_time: current_time,
            base: Default::default(),
        }
    }

    /// 更新统计信息的方法
    pub fn update_stats(
        &mut self,
        request_time: i64,
        response_time: i64,
        status_code: u16,
        result: RequestResult, // 使用枚举表示请求结果
    ) {
        // 增加总请求数
        self.total_requests += 1;

        // 计算请求延迟
        let latency = response_time - request_time;
        self.total_latency += latency;

        // 更新 HTTP 状态码统计
        // 很多爬虫都是使用0 代替；这里直接忽略0 的情况
        if status_code != 0 {
            *self
                .http_status_codes
                .entry(status_code.clone())
                .or_insert(0) += 1;
        }

        // 根据请求结果更新对应的统计数据
        match result {
            RequestResult::Successful => {
                self.successful_requests += 1;
            }
            RequestResult::SuccessfulAndCache => {
                self.successful_requests += 1;
                self.cache_hit += 1;
            }

            RequestResult::ParseError => {
                self.parse_errors += 1;
            }
            RequestResult::TimeoutError => {
                self.timeout_errors += 1;
            }
            RequestResult::ConnectionError => {
                self.connection_errors += 1;
            }
            RequestResult::StatusCodeError => {
                self.status_code_error += 1;
            }
        }
    }

    /// 将当前统计数据拼装到 `Stats` 结构体中，并清空当前统计数据
    pub fn to_stats_and_reset<'a>(&mut self, base: &'a StatsBase) -> Stats<'a> {
        // 获取当前时间作为结束时间
        let end_time = get_now_millis();

        // 构造时间周期
        let time_period = TimePeriod {
            start: self.start_time,
            end: end_time,
        };

        self.start_time = end_time;

        // 构造异常类型统计
        let exception_types = ExceptionTypes {
            connection_error: self.connection_errors,
            timeout_error: self.timeout_errors,
            parse_error: self.parse_errors,
            status_code_error: self.status_code_error,
        };

        // 计算错误率
        let error_rate = if self.total_requests > 0 {
            (self.parse_errors
                + self.timeout_errors
                + self.connection_errors
                + self.status_code_error) as f64
                / self.total_requests as f64
        } else {
            0.0
        };

        // 计算运行时长（从对象初始化到当前时间）
        let runtime_duration = (end_time - self.init_time) / 1000;

        let cache_hit_rate = if self.successful_requests == 0 {
            0.0
        } else {
            let cache_hit_rate = self.cache_hit as f64 / self.successful_requests as f64;
            (cache_hit_rate * 1000.0).round() / 1000.0
        };

        // ms
        let average_latency = (self.total_latency as f64 / self.total_requests as f64) / 1000.0;

        // 构造 `Stats` 结构体
        let stats = Stats {
            base,
            time_period,
            error_rate: (error_rate * 1000.0).round() / 1000.0,
            exception_types,
            runtime_duration,
            total_requests: self.total_requests,
            cache_hit_rate,            // 假设没有缓存相关数据，可以根据需要补充
            cache_hit: self.cache_hit, // 假设没有缓存相关数据，可以根据需要补充
            http_status_codes: self
                .http_status_codes
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
            average_request_latency: (average_latency * 1000.0).round() / 1000.0,
            hosts_ping_delay: HashMap::new(), // 假设没有主机延迟数据，可以根据需要补充
            system_resources: get_system_resources(),
        };

        stats
    }

    pub fn reset(&mut self) {
        self.base = Default::default();
    }
}

/// 获取系统资源数据
pub fn get_system_resources() -> SystemResources {
    // 创建一个 System 实例
    let mut system = System::new_all();

    // 刷新系统信息
    system.refresh_all();

    // 获取 CPU 使用率
    let cpu_usage = format!("{:.2}%", system.global_cpu_info().cpu_usage());

    // 获取内存使用情况（单位从 KB 转换为 MB）
    let total_memory = system.total_memory() / (1024 * 1024); // 总内存（MB）
    let used_memory = system.used_memory() / (1024 * 1024); // 已使用内存（MB）

    let memory_usage = Usage {
        used: used_memory,
        total: total_memory,
    };

    // 获取所有磁盘的使用情况（单位从字节转换为 MB）
    let mut total_disk_space = 0;
    let mut total_disk_used = 0;

    for disk in system.disks() {
        total_disk_space += disk.total_space() / (1024 * 1024); // 累加磁盘总空间（MB）
        total_disk_used += (disk.total_space() - disk.available_space()) / (1024 * 1024);
        // 累加磁盘已使用空间（MB）
    }

    let disk_usage = Usage {
        used: total_disk_used,
        total: total_disk_space,
    };

    // 构造 SystemResources
    SystemResources {
        cpu_usage,
        memory_usage,
        disk_usage,
    }
}

/// 测试tcp 连接耗时; 返回连接的耗时
pub fn run_test_tcp(addr: &str, port: u16, ping_timeout: Duration) -> Result<u64> {
    let sk = match addr.parse::<SocketAddr>() {
        Ok(sock) => sock,
        Err(_) => {
            let resolve_ip = IpAddr::from_str(addr)?;

            SocketAddr::new(resolve_ip, port)
        }
    };
    let start_time = Instant::now();
    let _ = TcpStream::connect_timeout(&sk, ping_timeout).map_err(|err| {
        anyhow!(
            "当前连接时长：{} ms;错误信息：{err}",
            start_time.elapsed().as_millis()
        )
    })?;
    let elapsed_time = start_time.elapsed();
    Ok(elapsed_time.as_micros() as u64)
}

#[cfg(test)]
mod tests {
    use crate::{
        get_system_resources, init_spider_vars, send_stats, RequestStatsConfig, StatsBase, GET_BASE,
    };
    use anyhow::Result;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn it_works() {
        // 1000XXXUSDT，10000XXXUSDT，1000000XXXUSDT 1MXXXUSDT

        // 获取系统资源数据
        let system_resources = get_system_resources();

        // 打印系统资源数据
        println!("系统资源数据: {:?}", system_resources);

        // 打印详细信息
        println!("CPU 使用率: {}", system_resources.cpu_usage);
        println!(
            "内存使用: 已使用 {} MB / 总计 {} MB",
            system_resources.memory_usage.used, system_resources.memory_usage.total
        );
        println!(
            "磁盘使用: 已使用 {} MB / 总计 {} MB",
            system_resources.disk_usage.used, system_resources.disk_usage.total
        );

        init_spider_vars(
            RequestStatsConfig {
                target: vec!["ws://35.79.121.103:5003".to_string()],
                reporting_cycle: Duration::from_secs(10000),
                host_test_port: 0,
            },
            Box::new(get_base),
            // Box::new(|| Ok(vec!["ssss".to_string()])),
            Box::new(get_hosts),
            None,
        )
        .unwrap();

        thread::sleep(Duration::from_secs(5));
        let base = GET_BASE();

        send_stats(&base, None);
    }

    fn get_base() -> StatsBase {
        let base = StatsBase {
            server_name: "".to_string(),
            scraper_name: "".to_string(),
            project_code: "".to_string(),
            scraper_type: "".to_string(),
            request_frequency: 0,
        };
        base
    }
    fn get_hosts() -> Result<Vec<String>> {
        Ok(vec!["ssss".to_string()])
    }
}
