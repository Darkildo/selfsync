//! Режимы запуска (раздел 13.4): собранный бинарь как подпроцесс.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use prost::Message;
use selfsync_core::blob;
use selfsync_core::hash::Hash;
use selfsync_core::path::VaultPath;
use selfsync_proto::v1 as pb;

const BIN: &str = env!("CARGO_BIN_EXE_selfsync");

fn add_token(data: &std::path::Path, vault: &str) -> String {
    let out = Command::new(BIN)
        .args(["token", "add", "--vault", vault, "--name", "dev", "--data"])
        .arg(data)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .find(|w| w.starts_with("ns_"))
        .unwrap()
        .to_owned()
}

struct CgiResp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CgiResp {
    fn header(&self, k: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    }
}

fn parse_cgi(out: &[u8]) -> CgiResp {
    let sep = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("нет конца заголовков");
    let head = std::str::from_utf8(&out[..sep]).unwrap();
    let mut status = 0;
    let mut headers = Vec::new();
    for line in head.split("\r\n") {
        let (k, v) = line.split_once(": ").unwrap();
        if k == "Status" {
            status = v.split_whitespace().next().unwrap().parse().unwrap();
        } else {
            headers.push((k.to_owned(), v.to_owned()));
        }
    }
    CgiResp {
        status,
        headers,
        body: out[sep + 4..].to_vec(),
    }
}

fn cgi(
    data: &std::path::Path,
    method: &str,
    path: &str,
    query: &str,
    token: Option<&str>,
    body: &[u8],
    extra: &[(&str, &str)],
) -> CgiResp {
    let mut cmd = Command::new(BIN);
    cmd.env_clear()
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REQUEST_METHOD", method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("HTTP_X_SELFSYNC_PROTO", "1")
        .env("SELFSYNC_DATA_DIR", data)
        .env("SELFSYNC_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(t) = token {
        cmd.env("HTTP_AUTHORIZATION", format!("Bearer {t}"));
    }
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(body).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_cgi(&out.stdout)
}

fn put_op(path: &str, h: Hash, size: u64) -> pb::Op {
    pb::Op {
        kind: Some(pb::op::Kind::Put(pb::Put {
            path: Some(VaultPath::parse(path).unwrap().to_proto()),
            base_rev: 0,
            hash: h.to_vec(),
            size,
            mtime: 0,
        })),
    }
}

#[test]
fn cgi_health_cold_start() {
    let data = tempfile::tempdir().unwrap();
    // разогрев кэша ФС для бинаря
    cgi(data.path(), "GET", "/v1/health", "", None, b"", &[]);
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let r = cgi(data.path(), "GET", "/v1/health", "", None, b"", &[]);
        best = best.min(t.elapsed());
        assert_eq!(r.status, 200);
        assert!(
            r.header("content-type")
                .unwrap()
                .starts_with("application/json")
        );
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["status"], "ok");
    }
    eprintln!("CGI /v1/health: лучший холодный старт {best:?}");
    // В release-сборке это единицы миллисекунд; в debug-тесте даём запас.
    assert!(best < Duration::from_millis(200), "{best:?}");
    // health не создаёт БД
    assert!(!data.path().join("server.db").exists());
}

#[test]
fn cgi_full_request_cycle_with_range() {
    let data = tempfile::tempdir().unwrap();
    let token = add_token(data.path(), "notes");
    let content = blob::encode(b"hello from cgi", None);
    let h = Hash::of(&content);
    let r = cgi(
        data.path(),
        "PUT",
        &format!("/v1/blobs/{}", h.to_hex()),
        "",
        Some(&token),
        &content,
        &[],
    );
    assert_eq!(r.status, 201);
    let ops = pb::OpsRequest {
        ops: vec![put_op("cgi.md", h, content.len() as u64)],
    };
    let r = cgi(
        data.path(),
        "POST",
        "/v1/ops",
        "",
        Some(&token),
        &ops.encode_to_vec(),
        &[("CONTENT_TYPE", "application/x-protobuf")],
    );
    assert_eq!(r.status, 200);
    let resp = pb::OpsResponse::decode(&r.body[..]).unwrap();
    assert!(matches!(
        resp.results[0].result,
        Some(pb::op_result::Result::Applied(_))
    ));
    let r = cgi(
        data.path(),
        "GET",
        "/v1/changes",
        "since=0",
        Some(&token),
        b"",
        &[],
    );
    let ch = pb::ChangesResponse::decode(&r.body[..]).unwrap();
    assert_eq!(ch.entries.len(), 1);
    // Range
    let r = cgi(
        data.path(),
        "GET",
        &format!("/v1/blobs/{}", h.to_hex()),
        "",
        Some(&token),
        b"",
        &[("HTTP_RANGE", "bytes=17-21")],
    );
    assert_eq!(r.status, 206);
    assert_eq!(r.body, b"hello");
    assert_eq!(
        r.header("content-range"),
        Some(format!("bytes 17-21/{}", content.len()).as_str())
    );
    // без токена
    let r = cgi(data.path(), "GET", "/v1/changes", "", None, b"", &[]);
    assert_eq!(r.status, 401);
    // путь через SCRIPT_NAME (caddy-cgi) и REQUEST_URI (fcgiwrap)
    let r = cgi(
        data.path(),
        "GET",
        "/changes",
        "since=0",
        Some(&token),
        b"",
        &[("SCRIPT_NAME", "/v1")],
    );
    assert_eq!(r.status, 200);
    let r = cgi(
        data.path(),
        "GET",
        "",
        "",
        Some(&token),
        b"",
        &[
            ("SCRIPT_NAME", "/usr/local/bin/selfsync"),
            ("REQUEST_URI", "/v1/stats"),
        ],
    );
    assert_eq!(r.status, 200);
    // wait в CGI отвечает сразу
    let t = Instant::now();
    let r = cgi(
        data.path(),
        "GET",
        "/v1/wait",
        "since=1&timeout=25",
        Some(&token),
        b"",
        &[],
    );
    assert_eq!(r.status, 200);
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn cgi_concurrent_writers_keep_seq_monotonic() {
    let data = tempfile::tempdir().unwrap();
    let token = add_token(data.path(), "notes");
    let content = blob::encode(b"same content", None);
    let h = Hash::of(&content);
    cgi(
        data.path(),
        "PUT",
        &format!("/v1/blobs/{}", h.to_hex()),
        "",
        Some(&token),
        &content,
        &[],
    );
    const PROCS: usize = 16;
    const PER: usize = 5;
    let handles: Vec<_> = (0..PROCS)
        .map(|p| {
            let data = data.path().to_path_buf();
            let token = token.clone();
            let content_len = content.len() as u64;
            std::thread::spawn(move || {
                for i in 0..PER {
                    let ops = pb::OpsRequest {
                        ops: vec![put_op(&format!("p{p}/f{i}.md"), h, content_len)],
                    };
                    let r = cgi(
                        &data,
                        "POST",
                        "/v1/ops",
                        "",
                        Some(&token),
                        &ops.encode_to_vec(),
                        &[],
                    );
                    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
                    let resp = pb::OpsResponse::decode(&r.body[..]).unwrap();
                    assert!(matches!(
                        resp.results[0].result,
                        Some(pb::op_result::Result::Applied(_))
                    ));
                }
            })
        })
        .collect();
    for hnd in handles {
        hnd.join().unwrap();
    }
    let r = cgi(
        data.path(),
        "GET",
        "/v1/changes",
        "since=0&limit=5000",
        Some(&token),
        b"",
        &[],
    );
    let ch = pb::ChangesResponse::decode(&r.body[..]).unwrap();
    let seqs: Vec<u64> = ch.entries.iter().map(|e| e.seq).collect();
    let expected: Vec<u64> = (1..=(PROCS * PER) as u64).collect();
    assert_eq!(
        seqs, expected,
        "seq строго монотонный, без пропусков и дублей"
    );
}

/// Запускает `selfsync socket`, передавая слушающий сокет как fd 3 (как systemd).
fn spawn_socket(listener: &TcpListener, data: &std::path::Path, idle: &str) -> Child {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let fd = listener.as_raw_fd();
    let mut cmd = Command::new(BIN);
    cmd.args(["socket", "--idle-timeout", idle, "--data"])
        .arg(data)
        .env("LISTEN_FDS", "1")
        .env_remove("LISTEN_PID")
        .env("SELFSYNC_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: между fork и exec вызываются только async-signal-safe dup2/fcntl.
    unsafe {
        cmd.pre_exec(move || {
            if fd != 3 {
                if libc::dup2(fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else {
                let flags = libc::fcntl(3, libc::F_GETFD);
                libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    cmd.spawn().unwrap()
}

fn http_get(addr: std::net::SocketAddr, path: &str, token: Option<&str>) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: x\r\nX-Selfsync-Proto: 1\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let sep = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (status, buf[sep + 4..].to_vec())
}

fn wait_exit(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Some(st) = child.try_wait().unwrap() {
            return Some(st);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn socket_activation_idle_exit_and_reactivation() {
    let data = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut child = spawn_socket(&listener, data.path(), "1s");
    let (st, body) = http_get(addr, "/v1/health", None);
    assert_eq!(st, 200);
    assert!(String::from_utf8_lossy(&body).contains("\"ok\""));
    // простой → выход с кодом 0
    let status =
        wait_exit(&mut child, Duration::from_secs(10)).expect("процесс не вышел по простою");
    assert!(status.success());
    // процесса нет, а сокет слушает (его держит «systemd» — тест): запрос ждёт в очереди
    let pending = std::thread::spawn(move || http_get(addr, "/v1/health", None));
    std::thread::sleep(Duration::from_millis(300));
    let mut child2 = spawn_socket(&listener, data.path(), "1s");
    let (st, _) = pending.join().unwrap();
    assert_eq!(st, 200, "следующий запрос обслужен новым процессом");
    assert!(
        wait_exit(&mut child2, Duration::from_secs(10))
            .unwrap()
            .success()
    );
}

#[test]
fn socket_long_request_not_cut_and_wait_does_not_hold() {
    let data = tempfile::tempdir().unwrap();
    let token = add_token(data.path(), "notes");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut child = spawn_socket(&listener, data.path(), "1s");

    // Долгая загрузка: тело приходит по кусочку дольше idle-таймаута.
    let content = vec![b'z'; 3000];
    let h = Hash::of(&content);
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "PUT /v1/blobs/{} HTTP/1.1\r\nHost: x\r\nX-Selfsync-Proto: 1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        h.to_hex(),
        content.len()
    )
    .unwrap();
    for part in content.chunks(1000) {
        s.write_all(part).unwrap();
        std::thread::sleep(Duration::from_millis(900));
    }
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.starts_with("HTTP/1.1 201"), "{text}");

    // Ожидающий long-poll не держит процесс: он выходит по простою, ответив на wait.
    let t = Instant::now();
    let (st, body) = http_get(addr, "/v1/wait?since=999&timeout=25", Some(&token));
    assert_eq!(st, 200);
    assert!(pb::VaultState::decode(&body[..]).is_ok());
    assert!(
        t.elapsed() < Duration::from_secs(10),
        "wait продержал процесс {:?}",
        t.elapsed()
    );
    let status = wait_exit(&mut child, Duration::from_secs(10)).expect("процесс не вышел");
    assert!(status.success());
}

#[test]
fn socket_without_systemd_fails_clearly() {
    let data = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(["socket", "--data"])
        .arg(data.path())
        .env_remove("LISTEN_FDS")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("systemd-socket-activate"), "{err}");
}

#[test]
fn systemd_socket_activate_tool() {
    let tool = "/usr/bin/systemd-socket-activate";
    if !std::path::Path::new(tool).exists() {
        eprintln!("systemd-socket-activate нет — пропуск");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = Command::new(tool)
        .args([
            "-l",
            &format!("127.0.0.1:{port}"),
            BIN,
            "socket",
            "--idle-timeout",
            "1s",
            "--data",
        ])
        .arg(data.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let start = Instant::now();
    let st = loop {
        if TcpStream::connect(addr).is_ok() {
            break http_get(addr, "/v1/health", None).0;
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(st, 200);
    let status = wait_exit(&mut child, Duration::from_secs(10)).expect("не вышел по простою");
    assert!(status.success());
}

#[test]
fn serve_unix_socket_and_healthcheck() {
    let data = tempfile::tempdir().unwrap();
    let sock = data.path().join("n.sock");
    let mut child = Command::new(BIN)
        .args(["serve", "--listen"])
        .arg(format!("unix:{}", sock.display()))
        .args(["--data"])
        .arg(data.path())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !sock.exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    let ok = Command::new(BIN)
        .args(["healthcheck", "--url"])
        .arg(format!("unix:{}", sock.display()))
        .status()
        .unwrap();
    assert!(ok.success());
    child.kill().unwrap();
    let _ = child.wait();
    let bad = Command::new(BIN)
        .args(["healthcheck", "--url", "http://127.0.0.1:1/v1/health"])
        .status()
        .unwrap();
    assert!(!bad.success());
}
