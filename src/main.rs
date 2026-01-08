use palc::Parser;
use regex::Regex;
use reqwest::{Client, Proxy};
use serde::Deserialize;
use std::fs;
use std::time::{Duration, Instant};
use tokio::time::timeout;

#[derive(Parser)]
#[command(name = "proxy-speedtest")]
#[command(long_about = "Test SingBox proxy nodes latency and download speed")]
struct Args {
    /// Path to the SingBox config JSON file
    config_path: String,
    /// Regex pattern to filter node tags (can specify multiple patterns)
    regexes: Vec<String>,
    /// Download test size in MB (optional, enables speed test if provided)
    #[arg(short = 'd', long = "download-mb")]
    download_mb: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Config {
    inbounds: Option<Vec<Inbound>>,
}

#[derive(Debug, Deserialize)]
struct Inbound {
    #[serde(rename = "type")]
    inbound_type: Option<String>,
    tag: Option<String>,
    listen_port: Option<u16>,
    listen: Option<String>,
}

#[derive(Debug, Clone)]
enum LatencyResult {
    Success {
        median: f64,
        average: f64,
        minimum: f64,
        maximum: f64,
    },
    Unstable(usize, usize), // valid_count, total_count
    AllFailed,
    SessionError(String),
}

#[derive(Debug, Clone)]
enum SpeedResult {
    Success(f64), // Speed in Mbps
    Failed(String),
}

#[derive(Debug, Clone)]
struct NodeResult {
    tag: String,
    port: u16,
    latency: LatencyResult,
    speed: Option<SpeedResult>,
}

impl std::fmt::Display for LatencyResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LatencyResult::Success {
                median,
                average,
                maximum,
                minimum,
            } => write!(f, "{median:.2}/{average:.2}/{minimum:.2}/{maximum:.2}"),
            LatencyResult::Unstable(valid, total) => write!(f, "Unstable ({}/{})", valid, total),
            LatencyResult::AllFailed => write!(f, "All Failed"),
            LatencyResult::SessionError(err) => write!(f, "Session Error: {}", err),
        }
    }
}

impl std::fmt::Display for SpeedResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpeedResult::Success(speed) => write!(f, "{:.2} Mbps", speed),
            SpeedResult::Failed(err) => write!(f, "Failed: {}", err),
        }
    }
}

async fn test_node_latency(port: u16, test_count: usize) -> LatencyResult {
    let url = "https://www.cloudflare.com/cdn-cgi/trace";
    let proxy_url = format!("socks5h://127.0.0.1:{}", port);

    let proxy = match Proxy::all(&proxy_url) {
        Ok(proxy) => proxy,
        Err(e) => return LatencyResult::SessionError(format!("Failed to create proxy: {}", e)),
    };

    let client = Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .build();

    let client = match client {
        Ok(client) => client,
        Err(e) => return LatencyResult::SessionError(format!("Failed to create client: {}", e)),
    };

    let mut latencies = Vec::new();

    println!("  预热连接...");
    let _ = timeout(Duration::from_secs(10), client.head(url).send()).await;

    for i in 0..test_count {
        let start = Instant::now();
        let result = timeout(Duration::from_secs(10), client.head(url).send()).await;

        match result {
            Ok(Ok(response)) => {
                if response.status().is_success() {
                    let elapsed_ms = start.elapsed().as_micros() as f64 / 1000.0;
                    latencies.push(elapsed_ms);
                    println!("  ↳ 第 {:2} 次: {:6.2} ms", i + 1, elapsed_ms);
                } else {
                    latencies.push(f64::INFINITY);
                    println!("  ↳ 第 {:2} 次: HTTP Error {}", i + 1, response.status());
                    break;
                }
            }
            Ok(Err(e)) => {
                latencies.push(f64::INFINITY);
                println!("  ↳ 第 {:2} 次: Error ({})", i + 1, e);
                break;
            }
            Err(_) => {
                latencies.push(f64::INFINITY);
                println!("  ↳ 第 {:2} 次: Timeout", i + 1);
                break;
            }
        }
    }

    if latencies.is_empty() || latencies.iter().all(|&l| l.is_infinite()) {
        return LatencyResult::AllFailed;
    }

    let valid_latencies: Vec<f64> = latencies
        .into_iter()
        .filter(|&l| !l.is_infinite())
        .collect();

    if valid_latencies.len() < 3 {
        return LatencyResult::Unstable(valid_latencies.len(), test_count);
    }

    let mut sorted = valid_latencies;
    sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    let average = sorted.iter().sum::<f64>() / sorted.len() as f64;

    LatencyResult::Success {
        median,
        average,
        minimum: *sorted.first().unwrap(),
        maximum: *sorted.last().unwrap(),
    }
}

async fn test_node_speed(port: u16, size_mb: u32) -> SpeedResult {
    let proxy_url = format!("socks5h://127.0.0.1:{}", port);

    let proxy = match Proxy::all(&proxy_url) {
        Ok(proxy) => proxy,
        Err(e) => return SpeedResult::Failed(format!("Failed to create proxy: {}", e)),
    };

    let client = Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(10))
        .build();

    let client = match client {
        Ok(client) => client,
        Err(e) => return SpeedResult::Failed(format!("Failed to create client: {}", e)),
    };

    let test_url = if size_mb <= 1024 {
        format!(
            "https://speed.cloudflare.com/__down?bytes={}",
            size_mb * 1024 * 1024
        )
    } else {
        return SpeedResult::Failed("Size too large (>1GB not supported)".to_string());
    };

    println!("  开始下载测试 ({} MB)...", size_mb);
    let start = Instant::now();

    let result = timeout(Duration::from_secs(120), client.get(test_url).send()).await;

    match result {
        Ok(Ok(response)) => {
            if response.status().is_success() {
                match response.bytes().await {
                    Ok(bytes) => {
                        let elapsed = start.elapsed();
                        let bytes_downloaded = bytes.len() as f64;
                        let megabits = (bytes_downloaded * 8.0) / 1_000_000.0;
                        let seconds = elapsed.as_secs_f64();
                        let speed_mbps = megabits / seconds;

                        println!(
                            "  ↳ 下载完成: {:.2} MiB in {:.2}s → {:.2} Mbps",
                            bytes_downloaded / 1024.0 / 1024.0,
                            seconds,
                            speed_mbps
                        );
                        SpeedResult::Success(speed_mbps)
                    }
                    Err(e) => SpeedResult::Failed(format!("Failed to read response: {}", e)),
                }
            } else {
                SpeedResult::Failed(format!("HTTP Error: {}", response.status()))
            }
        }
        Ok(Err(e)) => SpeedResult::Failed(format!("Request error: {}", e)),
        Err(_) => SpeedResult::Failed("Timeout".to_string()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Args {
        config_path,
        download_mb,
        regexes,
    } = Args::parse();

    let mut compiled_regexes = Vec::new();
    for pattern in &regexes {
        match Regex::new(pattern) {
            Ok(re) => compiled_regexes.push(re),
            Err(e) => {
                eprintln!("❌ 无效的正则表达式 '{}': {}", pattern, e);
                return Ok(());
            }
        }
    }

    let config_content = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("❌ 无法读取 JSON 文件: {}", e);
            return Ok(());
        }
    };

    let config: Config = match serde_json::from_str(&config_content) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("❌ JSON 解析失败: {}", e);
            return Ok(());
        }
    };

    let inbounds = match config.inbounds {
        Some(inbounds) => inbounds,
        None => {
            eprintln!("❌ 未找到 inbounds 字段");
            return Ok(());
        }
    };

    let mut socks_nodes = Vec::new();
    for inbound in inbounds {
        if let (Some(inbound_type), Some(tag), Some(port), listen) = (
            inbound.inbound_type,
            inbound.tag,
            inbound.listen_port,
            inbound.listen,
        ) {
            if inbound_type == "socks" {
                let listen_addr = listen.unwrap_or_else(|| "127.0.0.1".to_string());

                let tag_matches = if compiled_regexes.is_empty() {
                    true
                } else {
                    compiled_regexes.iter().all(|re| re.is_match(&tag))
                };

                if tag_matches && matches!(listen_addr.as_str(), "127.0.0.1" | "::1" | "localhost")
                {
                    socks_nodes.push((tag, port));
                }
            }
        }
    }

    if socks_nodes.is_empty() {
        if regexes.is_empty() {
            eprintln!("❌ 未找到任何 socks 类型的 inbound");
        } else {
            eprintln!("❌ 未找到匹配正则表达式的 socks 节点");
            eprintln!("   使用正则: {:?}", regexes);
        }
        return Ok(());
    }

    let test_description = if let Some(size) = download_mb {
        format!(
            "找到 {} 个 socks 节点，开始顺序测试（延迟测试10次 + 下载测试 {} MB）\n",
            socks_nodes.len(),
            size
        )
    } else {
        format!(
            "找到 {} 个 socks 节点，开始顺序测试（每节点10次延迟测试）\n",
            socks_nodes.len()
        )
    };

    println!("🚀 {}", test_description);
    println!("{}", "=".repeat(80));

    let mut results = Vec::new();

    for (idx, (tag, port)) in socks_nodes.iter().enumerate() {
        let current = idx + 1;
        let total = socks_nodes.len();

        println!(
            "📡 [{}/{}] 测试节点: {} (端口: {})",
            current, total, tag, port
        );

        print!("  延迟测试: ");
        let latency = test_node_latency(*port, 10).await;

        match &latency {
            LatencyResult::Success {
                median,
                average,
                minimum,
                maximum,
            } => {
                println!("✅ {median:.2}/{average:.2}/{minimum:.2}/{maximum:.2} ms");
            }
            LatencyResult::Unstable(valid, total) => {
                println!("⚠️  不稳定 ({}/{} 次成功)", valid, total);
            }
            LatencyResult::AllFailed => {
                println!("❌ 全部失败");
            }
            LatencyResult::SessionError(err) => {
                println!("❌ 连接错误: {}", err);
            }
        }

        let speed = if let Some(size_mb) = download_mb {
            println!("  速度测试:");
            let speed_result = test_node_speed(*port, size_mb).await;

            match &speed_result {
                SpeedResult::Success(mbps) => {
                    println!("  ✅ 下载速度: {:.2} Mbps", mbps);
                }
                SpeedResult::Failed(err) => {
                    println!("  ❌ 速度测试失败: {}", err);
                }
            }
            Some(speed_result)
        } else {
            None
        };

        results.push(NodeResult {
            tag: tag.clone(),
            port: *port,
            latency: latency.clone(),
            speed,
        });
        println!();
    }

    // 排序
    if download_mb.is_some() {
        results.sort_by(|a, b| match (&a.speed, &b.speed) {
            (Some(SpeedResult::Success(sa)), Some(SpeedResult::Success(sb))) => {
                sb.partial_cmp(sa).unwrap_or(std::cmp::Ordering::Equal)
            }
            (Some(SpeedResult::Success(_)), _) => std::cmp::Ordering::Less,
            (_, Some(SpeedResult::Success(_))) => std::cmp::Ordering::Greater,
            _ => match (&a.latency, &b.latency) {
                (
                    LatencyResult::Success { median: ma, .. },
                    LatencyResult::Success { median: mb, .. },
                ) => ma.partial_cmp(mb).unwrap_or(std::cmp::Ordering::Equal),
                (LatencyResult::Success { .. }, _) => std::cmp::Ordering::Less,
                (_, LatencyResult::Success { .. }) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            },
        });
    } else {
        results.sort_by(|a, b| match (&a.latency, &b.latency) {
            (
                LatencyResult::Success { median: ma, .. },
                LatencyResult::Success { median: mb, .. },
            ) => ma.partial_cmp(mb).unwrap_or(std::cmp::Ordering::Equal),
            (LatencyResult::Success { .. }, _) => std::cmp::Ordering::Less,
            (_, LatencyResult::Success { .. }) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        });
    }

    // 输出结果表格
    println!(
        "{}",
        "=".repeat(if download_mb.is_some() { 125 } else { 110 })
    );

    if download_mb.is_some() {
        println!(
            "{:<4} {:<8} {:<8} {:<8} {:<8} {:<8} {:<12} {:<45}",
            "排名", "端口", "med", "avg", "min", "max", "速度Mbps", "节点名称 (tag)"
        );
        println!("{}", "-".repeat(125));

        for (rank, result) in results.iter().enumerate() {
            let rank = rank + 1;
            match (&result.latency, result.speed.as_ref()) {
                (
                    LatencyResult::Success {
                        median,
                        average,
                        minimum,
                        maximum,
                    },
                    Some(SpeedResult::Success(speed)),
                ) => {
                    println!("{:<4} {:<10} {median:<8.2} {average:<8.2} {minimum:<8.2} {maximum:<8.2} {speed:<12.2} {:<45}", 
                             rank, result.port, result.tag);
                }
                (
                    LatencyResult::Success {
                        median,
                        average,
                        minimum,
                        maximum,
                    },
                    Some(SpeedResult::Failed(err)),
                ) => {
                    let err_display = if err.len() > 10 { &err[..10] } else { err };
                    println!("{:<4} {:<10} {median:<8.2} {average:<8.2} {minimum:<8.2} {maximum:<8.2} {err_display:<12} {:<45}", 
                             rank, result.port, result.tag);
                }
                _ => {
                    let speed_str = result
                        .speed
                        .as_ref()
                        .map(|s| format!("{}", s))
                        .unwrap_or_default();
                    println!(
                        "{:<4} {:<10} {:<35} {:<12} {:<45}",
                        rank, result.port, result.latency, speed_str, result.tag
                    );
                }
            }
        }
    } else {
        println!(
            "{:} {:<8} {:<8} {:<8} {:<8} {:<8} {:<45}",
            "排名", "端口", "med", "avg", "min", "max", "节点名称 (tag)"
        );
        println!("{}", "-".repeat(110));

        for (rank, result) in results.iter().enumerate() {
            let rank = rank + 1;
            match &result.latency {
                LatencyResult::Success {
                    median,
                    average,
                    minimum,
                    maximum,
                } => {
                    println!("{:<4} {:<10} {median:<8.2} {average:<8.2} {minimum:<8.2} {maximum:<8.2} {:<45}", 
                             rank, result.port, result.tag);
                }
                _ => {
                    println!(
                        "{:<4} {:<10} {:<35} {:<45}",
                        rank, result.port, result.latency, result.tag
                    );
                }
            }
        }
    }

    println!(
        "{}",
        "=".repeat(if download_mb.is_some() { 125 } else { 110 })
    );

    // 总结
    if let Some(size_mb) = download_mb {
        let successful = results
            .iter()
            .filter(|r| matches!(r.speed, Some(SpeedResult::Success(_))))
            .count();

        println!("\n📊 测试总结:");
        println!("   总节点数: {}", results.len());
        println!("   速度测试成功: {} 个", successful);
        println!("   速度测试失败: {} 个", results.len() - successful);
        println!("   测试文件大小: {} MB", size_mb);
    } else {
        println!(
            "\n📊 测试完成，共测试 {} 个节点（仅延迟测试）",
            results.len()
        );
    }

    Ok(())
}
