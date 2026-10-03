//! Maylin 高并发压测
//!
//! 用法: maylin.exe loadtest <url> [并发数] [总请求数] [token]
//! 示例:
//!   maylin.exe loadtest http://127.0.0.1:8000/ 100 10000
//!   maylin.exe loadtest http://127.0.0.1:7000/api/status 50 2000 <token>

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:8000/".into());
    let url = Arc::new(url);
    let conc: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50);
    let total: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let token = args.get(4).cloned();

    eprintln!(
        "压测目标 {url}  并发={conc}  总请求={total}"
    );

    let mut b = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(conc)
        .pool_idle_timeout(Duration::from_secs(120))
        .tcp_nodelay(true);
    if let Some(t) = &token {
        b = b.default_headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {t}")).unwrap(),
            );
            h
        });
    }
    let client = Arc::new(b.build().expect("构建 client 失败"));

    let ok = Arc::new(AtomicU64::new(0));
    let fail = Arc::new(AtomicU64::new(0));
    // 延迟桶（微秒）——每个 worker 收集，最后合并
    let per_worker = (total / conc as u64).max(1);
    let lat: Arc<std::sync::Mutex<Vec<f64>>> =
        Arc::new(std::sync::Mutex::new(Vec::with_capacity(total as usize)));

    let start = Instant::now();
    let mut handles = Vec::with_capacity(conc);
    for _ in 0..conc {
        let c = client.clone();
        let url = url.clone();
        let ok2 = ok.clone();
        let fail2 = fail.clone();
        let lat2 = lat.clone();
        handles.push(tokio::spawn(async move {
            let mut local_lat: Vec<f64> = Vec::with_capacity(per_worker as usize);
            for _ in 0..per_worker {
                let t0 = Instant::now();
                match c.get(url.as_str()).send().await {
                    Ok(r) if r.status().is_success() => {
                        ok2.fetch_add(1, Ordering::SeqCst);
                        local_lat.push(t0.elapsed().as_secs_f64() * 1000.0);
                    }
                    _ => {
                        fail2.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            lat2.lock().unwrap().extend(local_lat);
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    let elapsed = start.elapsed();

    let mut lats = lat.lock().unwrap().clone();
    lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| -> f64 {
        if lats.is_empty() {
            return 0.0;
        }
        let idx = ((lats.len() as f64 * p).ceil() as usize).saturating_sub(1);
        lats[idx]
    };
    let rps = total as f64 / elapsed.as_secs_f64();

    println!("=============== 压测报告 ===============");
    println!("目标        : {url}");
    println!("并发        : {conc}");
    println!("总请求      : {total}  (成功 {} / 失败 {})", ok.load(Ordering::SeqCst), fail.load(Ordering::SeqCst));
    println!("耗时        : {:.2?}", elapsed);
    println!("吞吐 RPS    : {rps:.0}");
    println!("延迟 p50    : {:.2} ms", pct(0.50));
    println!("延迟 p90    : {:.2} ms", pct(0.90));
    println!("延迟 p95    : {:.2} ms", pct(0.95));
    println!("延迟 p99    : {:.2} ms", pct(0.99));
    println!("延迟 max    : {:.2} ms", lats.last().copied().unwrap_or(0.0));
    println!("========================================");
}
