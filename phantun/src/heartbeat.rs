use crate::keepalive::{KeepaliveConfig, KeepaliveFailure};
use std::future::Future;
use tokio::sync::broadcast::{
    self,
    error::{RecvError, TryRecvError},
};
use tokio::time::{self, Instant};

/// 周期性探测连接，仅用对应编号的应答确认双向可达。
pub async fn monitor_heartbeat<F, Fut>(
    config: KeepaliveConfig,
    mut replies: broadcast::Receiver<u16>,
    mut send: F,
) -> KeepaliveFailure
where
    F: FnMut(u16) -> Fut,
    Fut: Future<Output = Option<()>>,
{
    // 关闭检测时永不发送探测。
    if config.time.is_zero() {
        return std::future::pending().await;
    }
    let mut counter = 0u16;
    loop {
        // 首次探测和成功后等待完整周期。
        time::sleep(config.time).await;
        let mut failures = 0;
        loop {
            // 清除历史应答，避免编号循环后误判。
            loop {
                match replies.try_recv() {
                    Ok(_) | Err(TryRecvError::Lagged(_)) => continue,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Closed) => return KeepaliveFailure::ReceiverClosed,
                }
            }
            let ticket = 0x8000 | (counter & 0x7fff);
            counter = counter.wrapping_add(1);
            if send(ticket).await.is_none() {
                return KeepaliveFailure::SendFailed;
            }
            let deadline = Instant::now() + config.interval;
            // 忽略其他编号和丢失通知，但不延长等待时间。
            let success = loop {
                match time::timeout_at(deadline, replies.recv()).await {
                    Ok(Ok(reply)) if reply == ticket => break true,
                    Ok(Ok(_)) | Ok(Err(RecvError::Lagged(_))) => continue,
                    Ok(Err(RecvError::Closed)) => return KeepaliveFailure::ReceiverClosed,
                    Err(_) => break false,
                }
            };
            if success {
                break;
            }
            failures += 1;
            if failures >= config.retries {
                return KeepaliveFailure::TimedOut;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// 验证错误和历史应答不能阻止超时，且每次重试使用新编号。
    #[tokio::test(start_paused = true)]
    async fn ignores_stale_replies_and_times_out() {
        // 首次探测前注入历史编号，之后注入不匹配的应答。
        let (tx, rx) = broadcast::channel(32);
        let (sent, mut tickets) = mpsc::unbounded_channel();
        tx.send(0x8000).unwrap();
        let task = tokio::spawn(monitor_heartbeat(
            KeepaliveConfig {
                time: Duration::from_secs(3),
                interval: Duration::from_secs(1),
                retries: 2,
            },
            rx,
            move |ticket| {
                sent.send(ticket).unwrap();
                async { Some(()) }
            },
        ));
        assert_eq!(tickets.recv().await, Some(0x8000));
        tx.send(0x8001).unwrap();
        assert_eq!(tickets.recv().await, Some(0x8001));
        assert_eq!(task.await.unwrap(), KeepaliveFailure::TimedOut);
    }

    /// 验证正确应答进入下一个完整心跳周期。
    #[tokio::test(start_paused = true)]
    async fn correct_reply_starts_next_cycle() {
        // 应答第一个编号后，确认后续探测使用新的编号。
        let (tx, rx) = broadcast::channel(32);
        let (sent, mut tickets) = mpsc::unbounded_channel();
        let task = tokio::spawn(monitor_heartbeat(
            KeepaliveConfig {
                time: Duration::from_secs(3),
                interval: Duration::from_secs(1),
                retries: 2,
            },
            rx,
            move |ticket| {
                sent.send(ticket).unwrap();
                async { Some(()) }
            },
        ));
        assert_eq!(tickets.recv().await, Some(0x8000));
        tx.send(0x8000).unwrap();
        assert_eq!(tickets.recv().await, Some(0x8001));
        task.abort();
    }

    /// 验证关闭、发送失败和通道关闭时的监控行为。
    #[tokio::test(start_paused = true)]
    async fn disabled_and_failure_paths() {
        // 关闭检测时不会调用发送；开启后分别验证失败原因。
        let (tx, rx) = broadcast::channel(32);
        let task = tokio::spawn(monitor_heartbeat(
            KeepaliveConfig {
                time: Duration::ZERO,
                interval: Duration::from_secs(1),
                retries: 1,
            },
            rx,
            |_| async { panic!("disabled heartbeat sent") },
        ));
        time::advance(Duration::from_secs(100)).await;
        assert!(!task.is_finished());
        task.abort();
        let rx = tx.subscribe();
        assert_eq!(
            monitor_heartbeat(
                KeepaliveConfig {
                    time: Duration::from_secs(1),
                    interval: Duration::from_secs(1),
                    retries: 1
                },
                rx,
                |_| async { None }
            )
            .await,
            KeepaliveFailure::SendFailed
        );
        let rx = tx.subscribe();
        drop(tx);
        assert_eq!(
            monitor_heartbeat(
                KeepaliveConfig {
                    time: Duration::from_secs(1),
                    interval: Duration::from_secs(1),
                    retries: 1
                },
                rx,
                |_| async { Some(()) }
            )
            .await,
            KeepaliveFailure::ReceiverClosed
        );
    }
    /// 验证心跳编号低 15 位循环，高位始终保持标识。
    #[tokio::test(start_paused = true)]
    async fn ticket_wraps_at_boundary() {
        // 连续应答至编号回绕，检查边界两侧的编号。
        let (tx, rx) = broadcast::channel(32);
        let (sent, mut tickets) = mpsc::unbounded_channel();
        let task = tokio::spawn(monitor_heartbeat(
            KeepaliveConfig {
                time: Duration::from_nanos(1),
                interval: Duration::from_secs(1),
                retries: 1,
            },
            rx,
            move |ticket| {
                sent.send(ticket).unwrap();
                async { Some(()) }
            },
        ));
        for n in 0..=0x8000u32 {
            let ticket = tickets.recv().await.unwrap();
            assert_eq!(ticket, 0x8000 | (n as u16 & 0x7fff));
            tx.send(ticket).unwrap();
        }
        task.abort();
    }
}
