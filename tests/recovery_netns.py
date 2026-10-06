#!/usr/bin/env python3
"""在两个临时 network namespace 中验证 Phantun 失效连接恢复业务。"""

import argparse
import json
import os
import re
import select
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


COMMAND_TIMEOUT = 15
LOG_TAIL_LINES = 80
CLIENT_IP = "10.203.0.1"
SERVER_IP = "10.203.0.2"


DRIVER_CODE = r'''
import json
import select
import socket
import sys
import time

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.bind(("127.0.0.1", 23456))
sock.setblocking(False)

for line in sys.stdin:
    command = json.loads(line)
    if command["op"] == "roundtrip":
        phase = command["phase"]
        end = time.monotonic() + command["duration"]
        sent = 0
        matched = 0
        expected = {}
        while time.monotonic() < end:
            payload = json.dumps({"phase": phase, "count": sent}, separators=(",", ":")).encode()
            expected[payload] = True
            sock.sendto(payload, ("127.0.0.1", 1234))
            sent += 1
            wait_until = time.monotonic() + 0.1
            while time.monotonic() < wait_until:
                ready, _, _ = select.select([sock], [], [], max(0, wait_until - time.monotonic()))
                if not ready:
                    break
                data, _ = sock.recvfrom(65535)
                if data in expected:
                    matched += 1
                    del expected[data]
            time.sleep(max(0, min(0.1, end - time.monotonic())))
        print(json.dumps({"op": "roundtrip", "phase": phase, "matched": matched, "sent": sent}), flush=True)
    elif command["op"] == "idle":
        end = time.monotonic() + command["duration"]
        unexpected = 0
        while time.monotonic() < end:
            ready, _, _ = select.select([sock], [], [], max(0, end - time.monotonic()))
            if ready:
                sock.recvfrom(65535)
                unexpected += 1
        print(json.dumps({"op": "idle", "unexpected": unexpected}), flush=True)
    else:
        raise RuntimeError("unknown driver operation")
'''


ECHO_CODE = r'''
import socket

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.bind(("127.0.0.1", 1234))
while True:
    data, address = sock.recvfrom(65535)
    sock.sendto(data, address)
'''


def run_command(command, check=True):
    """执行一次 namespace 配置命令并在超时或失败时返回明确错误。"""
    completed = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=COMMAND_TIMEOUT,
    )
    if check and completed.returncode != 0:
        raise RuntimeError(f"command failed ({completed.returncode}): {' '.join(command)}\n{completed.stderr}")
    return completed


def namespace_command(namespace, command):
    """将命令限制在本次创建的单个 network namespace 中执行。"""
    return ["ip", "netns", "exec", namespace, *command]


def start_logged(command, log_path, pipe_stdout=False):
    """启动一个带独立日志文件的 namespace 进程并返回其句柄。"""
    log_file = open(log_path, "w", encoding="utf-8")
    process = subprocess.Popen(
        command,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE if pipe_stdout else log_file,
        stderr=log_file if pipe_stdout else subprocess.STDOUT,
        start_new_session=True,
        text=True,
    )
    process.log_file = log_file
    return process


def wait_for_log(log_path, text, timeout=COMMAND_TIMEOUT):
    """等待进程日志出现启动标志，避免阶段命令抢跑。"""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if log_path.exists() and text in log_path.read_text(encoding="utf-8", errors="replace"):
            return
        time.sleep(0.1)
    raise RuntimeError(f"log did not contain {text!r}: {log_path}")


def send_driver(driver, command):
    """向固定 UDP 来源端口驱动发送阶段命令并解析一行 JSON 结果。"""
    driver.stdin.write(json.dumps(command) + "\n")
    driver.stdin.flush()
    wait_timeout = command.get("duration", COMMAND_TIMEOUT) + COMMAND_TIMEOUT
    ready, _, _ = select.select([driver.stdout], [], [], wait_timeout)
    if not ready:
        raise RuntimeError(
            f"driver timed out waiting for op={command.get('op')} phase={command.get('phase')}"
        )
    line = driver.stdout.readline()
    if not line:
        raise RuntimeError(
            f"driver exited before returning op={command.get('op')} phase={command.get('phase')}"
        )
    return json.loads(line)


def count_connections(log_path):
    """统计 server 已建立 fake TCP 连接的日志数量。"""
    if not log_path.exists():
        return 0
    return len(re.findall(r"New connection:", log_path.read_text(encoding="utf-8", errors="replace")))


def source_port(log_path):
    """从 server 首条连接日志提取来自 client namespace 的 TCP 源端口。"""
    content = log_path.read_text(encoding="utf-8", errors="replace")
    match = re.search(rf"New connection: .*?{re.escape(CLIENT_IP)}:(\d+)", content)
    if not match:
        raise RuntimeError("unable to find the original client TCP source port in server log")
    return int(match.group(1))


def stop_process(process):
    """终止本次测试启动的进程组并关闭其日志句柄。"""
    if process is None:
        return
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=COMMAND_TIMEOUT)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=COMMAND_TIMEOUT)
    if process.stdin is not None:
        process.stdin.close()
    if process.stdout is not None:
        process.stdout.close()
    log_file = getattr(process, "log_file", None)
    if log_file is not None:
        log_file.close()


def tail(log_path):
    """返回有限的失败日志尾，避免失败报告无限增长。"""
    if not log_path.exists():
        return ""
    return "\n".join(log_path.read_text(encoding="utf-8", errors="replace").splitlines()[-LOG_TAIL_LINES:])


def require_environment():
    """检查 root 权限及本测试允许使用的系统命令。"""
    if os.geteuid() != 0:
        raise RuntimeError("recovery_netns.py requires root")
    for command in ("ip", "iptables"):
        if shutil.which(command) is None:
            raise RuntimeError(f"missing required command: {command}")


def main():
    """创建隔离网络、运行健康/故障/恢复阶段并打印 JSON 结果。"""
    parser = argparse.ArgumentParser(description="Phantun stale connection recovery test")
    parser.add_argument("--client", required=True, type=Path)
    parser.add_argument("--server", required=True, type=Path)
    parser.add_argument("--baseline-client", action="store_true")
    args = parser.parse_args()
    require_environment()
    client_binary = args.client.resolve()
    server_binary = args.server.resolve()
    if not client_binary.is_file() or not server_binary.is_file():
        raise RuntimeError("client and server must resolve to existing files")

    client_namespace = f"phrec-{os.getpid()}-c"
    server_namespace = f"phrec-{os.getpid()}-s"
    processes = []
    client_log = None
    server_log = None
    driver_log = None
    temporary_directory = tempfile.TemporaryDirectory(prefix="phantun-recovery-")
    root = Path(temporary_directory.name)
    client_log = root / "client.log"
    server_log = root / "server.log"
    driver_log = root / "driver.log"
    try:
            run_command(["ip", "netns", "add", client_namespace])
            run_command(["ip", "netns", "add", server_namespace])
            run_command(namespace_command(client_namespace, ["ip", "link", "add", "eth0", "type", "veth", "peer", "name", "eth0", "netns", server_namespace]))
            run_command(namespace_command(client_namespace, ["ip", "link", "set", "lo", "up"]))
            run_command(namespace_command(server_namespace, ["ip", "link", "set", "lo", "up"]))
            for namespace, address in ((client_namespace, CLIENT_IP), (server_namespace, SERVER_IP)):
                run_command(namespace_command(namespace, ["ip", "addr", "add", f"{address}/24", "dev", "eth0"]))
                run_command(namespace_command(namespace, ["ip", "link", "set", "eth0", "up"]))
                run_command(namespace_command(namespace, ["sysctl", "-w", "net.ipv4.ip_forward=1"]))
            run_command(namespace_command(client_namespace, ["iptables", "-t", "nat", "-A", "POSTROUTING", "-s", "192.168.200.2", "-o", "eth0", "-j", "MASQUERADE"]))
            run_command(namespace_command(server_namespace, ["iptables", "-t", "nat", "-A", "PREROUTING", "-p", "tcp", "--dport", "4567", "-j", "DNAT", "--to-destination", "192.168.201.2"]))

            echo = start_logged(namespace_command(server_namespace, ["python3", "-u", "-c", ECHO_CODE]), root / "echo.log")
            processes.append(echo)
            server = start_logged(namespace_command(server_namespace, ["env", "RUST_LOG=info", str(server_binary), "--local", "4567", "--remote", "127.0.0.1:1234", "--ipv4-only"]), server_log)
            processes.append(server)
            wait_for_log(server_log, "Listening on")
            client_arguments = [str(client_binary), "--local", "127.0.0.1:1234", "--remote", "10.203.0.2:4567", "--ipv4-only"]
            if not args.baseline_client:
                client_arguments.extend(["--keepalive-time", "3", "--keepalive-interval", "1", "--keepalive-retries", "2"])
            client = start_logged(namespace_command(client_namespace, ["env", "RUST_LOG=info", *client_arguments]), client_log)
            processes.append(client)
            wait_for_log(client_log, "Created TUN device")
            driver = start_logged(namespace_command(client_namespace, ["python3", "-u", "-c", DRIVER_CODE]), driver_log, pipe_stdout=True)
            processes.append(driver)

            healthy = send_driver(driver, {"op": "roundtrip", "phase": "healthy", "duration": 3})
            print(json.dumps({"stage": "healthy", **healthy}))
            if healthy["matched"] < 5 or count_connections(server_log) != 1:
                raise RuntimeError("healthy roundtrip did not establish exactly one connection")
            original_pid = client.pid

            idle = send_driver(driver, {"op": "idle", "duration": 9})
            print(json.dumps({"stage": "idle", **idle}))
            if idle["unexpected"] != 0 or count_connections(server_log) != 1 or client.poll() is not None:
                raise RuntimeError("healthy idle phase was not stable")
            recovered = send_driver(driver, {"op": "roundtrip", "phase": "pre-fault", "duration": 2})
            print(json.dumps({"stage": "pre-fault", **recovered}))
            if recovered["matched"] < 5:
                raise RuntimeError("pre-fault roundtrip failed")

            old_port = source_port(server_log)
            old_drop = ["iptables", "-I", "FORWARD", "1", "-p", "tcp", "--sport", str(old_port), "--dport", "4567", "-j", "DROP"]
            all_drop = ["iptables", "-I", "FORWARD", "1", "-p", "tcp", "--dport", "4567", "-j", "DROP"]
            run_command(namespace_command(client_namespace, old_drop))
            run_command(namespace_command(client_namespace, all_drop))
            fault = send_driver(driver, {"op": "roundtrip", "phase": "fault", "duration": 7})
            print(json.dumps({"stage": "fault", **fault}))
            if fault["matched"] != 0 or client.poll() is not None:
                raise RuntimeError("fault phase unexpectedly returned traffic or stopped client")
            client_log_text = client_log.read_text(encoding="utf-8", errors="replace")
            if args.baseline_client:
                if "Keepalive failed" in client_log_text:
                    raise RuntimeError("baseline client unexpectedly reported keepalive failure")
            elif "Keepalive failed" not in client_log_text:
                raise RuntimeError("fixed client did not report keepalive failure")

            run_command(namespace_command(client_namespace, ["iptables", "-D", "FORWARD", "-p", "tcp", "--dport", "4567", "-j", "DROP"]))
            post_fault = send_driver(driver, {"op": "roundtrip", "phase": "recovery", "duration": 12})
            print(json.dumps({"stage": "recovery", **post_fault}))
            connection_count = count_connections(server_log)
            if args.baseline_client:
                if post_fault["matched"] != 0 or connection_count != 1 or client.poll() is not None:
                    raise RuntimeError("baseline client did not reproduce the stale connection")
            elif post_fault["matched"] < 5 or connection_count < 2 or client.poll() is not None:
                raise RuntimeError("fixed client did not establish a replacement connection")
            if client.pid != original_pid:
                raise RuntimeError("client process changed during recovery")
            print(json.dumps({"stage": "complete", "connections": connection_count, "client_pid": client.pid}))
    except Exception:
        if client_log is not None:
            print(tail(client_log), file=sys.stderr)
        if server_log is not None:
            print(tail(server_log), file=sys.stderr)
        raise
    finally:
        for process in reversed(processes):
            stop_process(process)
        for namespace in (client_namespace, server_namespace):
            subprocess.run(["ip", "netns", "del", namespace], check=False, timeout=COMMAND_TIMEOUT)
        temporary_directory.cleanup()


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"recovery test failed: {error}", file=sys.stderr)
        raise SystemExit(1)
