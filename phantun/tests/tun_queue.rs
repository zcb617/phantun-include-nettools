use phantun::utils::ensure_tun_tx_queue_len;
use std::io;
use tokio_tun::TunBuilder;

/// Checks invalid queue requests and interface names before any privileged ioctl is attempted.
#[test]
fn tun_queue_rejects_invalid_input() {
    assert_eq!(
        ensure_tun_tx_queue_len("ptq0", 0).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        ensure_tun_tx_queue_len("ptq0", -1).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        ensure_tun_tx_queue_len("", 4096).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );

    let overlong_name = "x".repeat(nix::libc::IFNAMSIZ);
    assert_eq!(
        ensure_tun_tx_queue_len(&overlong_name, 4096)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        ensure_tun_tx_queue_len("ptq\0x", 4096).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

/// Verifies that a real TUN queue is raised to each requested minimum and never lowered.
#[tokio::test]
#[ignore = "requires CAP_NET_ADMIN and /dev/net/tun; run explicitly in isolated container"]
async fn tun_queue_preserves_minimum() {
    let device_name = format!("ptq{}", std::process::id());
    let tun = TunBuilder::new()
        .name(&device_name)
        .up()
        .queues(1)
        .build()
        .unwrap();

    assert!(ensure_tun_tx_queue_len(tun[0].name(), 4096).unwrap() >= 4096);
    assert!(ensure_tun_tx_queue_len(tun[0].name(), 8192).unwrap() >= 8192);
    assert!(ensure_tun_tx_queue_len(tun[0].name(), 4096).unwrap() >= 8192);

    drop(tun);
}

/// Confirms ioctl errors are returned for a short name that does not identify an interface.
#[test]
fn tun_queue_returns_missing_interface_error() {
    let process_id = std::process::id();
    let mut missing_name = None;
    for suffix in 0..100 {
        let candidate = format!("ptq{process_id}{suffix:02}");
        if nix::net::if_::if_nametoindex(candidate.as_str()).is_err() {
            missing_name = Some(candidate);
            break;
        }
    }
    let missing_name = missing_name.expect("no unused short TUN interface name was found");

    assert!(ensure_tun_tx_queue_len(&missing_name, 4096).is_err());
}
