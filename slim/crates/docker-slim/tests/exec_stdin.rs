//! Real CLI against a bounded Engine API fixture. Output arrives only after
//! stdin EOF, as it does for batch SQL, hashes and `cat` in a container.
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn request(stream: &mut TcpStream) -> (String, String) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
        assert!(header.len() < 8192);
    }
    let header = String::from_utf8(header).unwrap();
    let length = header
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    (header, String::from_utf8(body).unwrap())
}

fn respond(stream: &mut TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
}

fn run(input: Vec<u8>, exit_code: i32) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let expected = input.clone();
    let server = thread::spawn(move || {
        let accept = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => return stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("fixture accept failed: {e}"),
                }
            }
        };
        let mut create = accept();
        let (header, body) = request(&mut create);
        assert!(header.starts_with("POST /v1.43/containers/fixture/exec "));
        assert!(body.contains("\"AttachStdin\":true"));
        assert!(body.contains("\"Cmd\":[\"cat\"]"));
        respond(&mut create, r#"{"Id":"test-exec"}"#);
        drop(create);

        let mut stream = accept();
        let (header, _) = request(&mut stream);
        assert!(header.contains("/exec/test-exec/start "));
        stream
            .write_all(b"HTTP/1.1 101 UPGRADED\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n")
            .unwrap();
        let mut received = Vec::new();
        stream
            .read_to_end(&mut received)
            .expect("exec must send stdin EOF");
        assert_eq!(received, expected);
        // The input half is closed, but output must remain readable.
        let payload = b"received stdin through EOF\n";
        let mut frame = vec![1, 0, 0, 0];
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        stream.write_all(&frame).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        drop(stream);

        let mut inspect = accept();
        let (header, _) = request(&mut inspect);
        assert!(header.contains("/exec/test-exec/json "));
        respond(
            &mut inspect,
            &format!(r#"{{"ExitCode":{exit_code},"Running":false}}"#),
        );
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_docker-slim"))
        .args(["exec", "-i", "fixture", "cat"])
        .env("DOCKER_HOST", endpoint)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(exit_code));
    assert_eq!(output.stdout, b"received stdin through EOF\n");
}

#[test]
fn exec_empty_stdin_delivers_eof_and_drains_output() {
    run(Vec::new(), 0);
}

#[test]
fn exec_multichunk_stdin_and_nonzero_exit() {
    run(vec![b'x'; 32 * 1024], 7);
}
