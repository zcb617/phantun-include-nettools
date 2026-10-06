use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{self, Instant};

/// 配置单条连接健康探测的超时、间隔和最大探测次数。
#[derive(Clone, Copy, Debug)]
pub struct KeepaliveConfig {
    /// 允许远端没有输入的最长时间。
    pub time: Duration,
    /// 两次健康探测之间等待远端响应的时间。
    pub interval: Duration,
    /// 连续没有响应时允许发送的探测次数。
    pub retries: u32,
}

/// 表示健康探测结束的具体原因。
#[derive(Debug, PartialEq, Eq)]
pub enum KeepaliveFailure {
    /// 所有探测都未收到远端输入。
    TimedOut,
    /// 本地无法写入探测报文。
    SendFailed,
    /// 连接的远端输入通知已关闭。
    ReceiverClosed,
}

/// 监控远端输入并在失去响应时返回连接失效原因。
pub async fn monitor_keepalive<F, Fut>(
    config: KeepaliveConfig,
    mut received: watch::Receiver<Instant>,
    mut probe: F,
) -> KeepaliveFailure
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<()>>,
{
    if config.time.is_zero() {
        return std::future::pending::<KeepaliveFailure>().await;
    }

    let mut attempts = 0;
    let mut deadline = *received.borrow_and_update() + config.time;

    loop {
        tokio::select! {
            biased;
            changed = received.changed() => {
                if changed.is_err() {
                    return KeepaliveFailure::ReceiverClosed;
                }
                attempts = 0;
                deadline = *received.borrow() + config.time;
            }
            _ = time::sleep_until(deadline) => {
                if attempts >= config.retries {
                    return KeepaliveFailure::TimedOut;
                }
                if probe().await.is_none() {
                    return KeepaliveFailure::SendFailed;
                }
                attempts += 1;
                deadline = Instant::now() + config.interval;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn config(time: u64, interval: u64, retries: u32) -> KeepaliveConfig {
        KeepaliveConfig {
            time: Duration::from_secs(time),
            interval: Duration::from_secs(interval),
            retries,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_without_remote_replies() {
        let (tx, rx) = watch::channel(Instant::now());
        let probes = Arc::new(AtomicUsize::new(0));
        let count = probes.clone();
        let task = tokio::spawn(monitor_keepalive(config(3, 1, 2), rx, move || {
            count.fetch_add(1, Ordering::SeqCst);
            async { Some(()) }
        }));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(probes.load(Ordering::SeqCst), 2);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(task.await.unwrap(), KeepaliveFailure::TimedOut);
        drop(tx);
    }

    #[tokio::test(start_paused = true)]
    async fn remote_reply_resets_probe_deadline() {
        let (tx, rx) = watch::channel(Instant::now());
        let probes = Arc::new(AtomicUsize::new(0));
        let count = probes.clone();
        let task = tokio::spawn(monitor_keepalive(config(3, 1, 2), rx, move || {
            count.fetch_add(1, Ordering::SeqCst);
            async { Some(()) }
        }));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        tx.send(Instant::now()).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(probes.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn send_failure_ends_monitor_immediately() {
        let (_tx, rx) = watch::channel(Instant::now());
        let probes = Arc::new(AtomicUsize::new(0));
        let count = probes.clone();
        let task = tokio::spawn(monitor_keepalive(config(3, 1, 2), rx, move || {
            count.fetch_add(1, Ordering::SeqCst);
            async { None }
        }));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(task.await.unwrap(), KeepaliveFailure::SendFailed);
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_monitor_never_probes() {
        let (_tx, rx) = watch::channel(Instant::now());
        let probes = Arc::new(AtomicUsize::new(0));
        let count = probes.clone();
        let task = tokio::spawn(monitor_keepalive(config(0, 1, 2), rx, move || {
            count.fetch_add(1, Ordering::SeqCst);
            async { Some(()) }
        }));
        tokio::time::advance(Duration::from_secs(3600)).await;
        tokio::task::yield_now().await;
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert!(!task.is_finished());
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn last_probe_waits_for_reply() {
        let (tx, rx) = watch::channel(Instant::now());
        let probes = Arc::new(AtomicUsize::new(0));
        let count = probes.clone();
        let task = tokio::spawn(monitor_keepalive(config(3, 1, 2), rx, move || {
            count.fetch_add(1, Ordering::SeqCst);
            async { Some(()) }
        }));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(500)).await;
        tx.send(Instant::now()).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(500)).await;
        tokio::task::yield_now().await;
        assert_eq!(probes.load(Ordering::SeqCst), 2);
        assert!(!task.is_finished());
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn closed_receiver_ends_monitor() {
        let (tx, rx) = watch::channel(Instant::now());
        let task = tokio::spawn(monitor_keepalive(config(3, 1, 2), rx, || async {
            Some(())
        }));
        tokio::task::yield_now().await;
        drop(tx);
        assert_eq!(task.await.unwrap(), KeepaliveFailure::ReceiverClosed);
    }
}
