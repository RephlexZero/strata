#[cfg(unix)]
#[test]
fn strata_probe_exits_cleanly_on_sigint() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let bin = env!("CARGO_BIN_EXE_strata-probe");
    let mut child = Command::new(bin)
        .arg("--bind")
        .arg("127.0.0.1:0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_millis(250));

    let pid = child.id() as libc::pid_t;
    // SAFETY: `child.id()` is the OS PID for the spawned test subprocess.
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "probe exited with unexpected status: {status}"
            );
            return;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for strata-probe to exit after SIGINT"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
