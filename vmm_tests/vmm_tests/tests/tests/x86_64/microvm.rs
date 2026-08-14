// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use base64::Engine;
use mesh::CancelContext;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::process::Child;
use std::process::ChildStdin;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test_no_agent;
use vmm_test_macros::vmm_test_with;

const MICROVM_BOOT_MARKER: &[u8] = b"ALPINE-MICROVM-BOOT-OK";
const PHASE_2_TIMEOUT: Duration = Duration::from_secs(60);
const PHASE_3_TX_COUNT: usize = 10_000;

struct OpenvmmTestProcess {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    output_recv: mpsc::Receiver<Vec<u8>>,
    output: Vec<u8>,
}

impl OpenvmmTestProcess {
    fn launch(executable: &Path, args: &[OsString]) -> anyhow::Result<Self> {
        let mut child = Command::new(executable)
            .args(args)
            .env("OPENVMM_LOG", "off")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to launch OpenVMM")?;
        let stdin = child
            .stdin
            .take()
            .context("failed to capture OpenVMM stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to capture OpenVMM stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("failed to capture OpenVMM stderr")?;
        let (output_send, output_recv) = mpsc::channel();
        for mut stream in [Box::new(stdout) as Box<dyn Read + Send>, Box::new(stderr)] {
            let output_send = output_send.clone();
            thread::spawn(move || {
                let mut chunk = vec![0; 4096];
                while let Ok(count) = stream.read(&mut chunk) {
                    if count == 0 || output_send.send(chunk[..count].to_vec()).is_err() {
                        break;
                    }
                }
            });
        }
        drop(output_send);

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            output_recv,
            output: Vec::new(),
        })
    }

    fn send_line(&mut self, line: &str) -> anyhow::Result<()> {
        let stdin = self.stdin.as_mut().context("OpenVMM stdin is closed")?;
        writeln!(stdin, "{line}").context("failed to write guest command")?;
        stdin.flush().context("failed to flush guest command")
    }

    fn wait_for(&mut self, marker: &[u8]) -> anyhow::Result<()> {
        let started = Instant::now();
        loop {
            if contains_bytes(&self.output, marker) {
                return Ok(());
            }
            if let Some(status) = self.child_mut()?.try_wait()? {
                anyhow::bail!(
                    "OpenVMM exited with {status} before marker {:?}; output: {}",
                    String::from_utf8_lossy(marker),
                    output_tail(&self.output)
                );
            }
            let remaining = PHASE_2_TIMEOUT.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                anyhow::bail!(
                    "timed out waiting for guest marker; output: {}",
                    output_tail(&self.output)
                );
            }
            match self
                .output_recv
                .recv_timeout(remaining.min(Duration::from_millis(250)))
            {
                Ok(chunk) => self.output.extend_from_slice(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("OpenVMM output closed before guest marker")
                }
            }
        }
    }

    fn wait_for_output_line(&mut self, marker: &[u8]) -> anyhow::Result<()> {
        let started = Instant::now();
        loop {
            if count_output_lines(&self.output, marker) != 0 {
                return Ok(());
            }
            if let Some(status) = self.child_mut()?.try_wait()? {
                anyhow::bail!(
                    "OpenVMM exited with {status} before output line {:?}; output: {}",
                    String::from_utf8_lossy(marker),
                    output_tail(&self.output)
                );
            }
            let remaining = PHASE_2_TIMEOUT.saturating_sub(started.elapsed());
            anyhow::ensure!(
                !remaining.is_zero(),
                "timed out waiting for guest output line"
            );
            match self
                .output_recv
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(chunk) => self.output.extend_from_slice(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = self.child_mut()?.wait()?;
                    anyhow::bail!(
                        "OpenVMM exited with {status} before output line {:?}; output: {}",
                        String::from_utf8_lossy(marker),
                        output_tail(&self.output)
                    )
                }
            }
        }
    }

    fn wait(mut self) -> anyhow::Result<(ExitStatus, Vec<u8>)> {
        self.stdin.take();
        let started = Instant::now();
        loop {
            while let Ok(chunk) = self.output_recv.try_recv() {
                self.output.extend_from_slice(&chunk);
            }
            if let Some(status) = self.child_mut()?.try_wait()? {
                self.child.take();
                while let Ok(chunk) = self.output_recv.recv_timeout(Duration::from_millis(100)) {
                    self.output.extend_from_slice(&chunk);
                }
                return Ok((status, std::mem::take(&mut self.output)));
            }
            if started.elapsed() >= PHASE_2_TIMEOUT {
                anyhow::bail!(
                    "timed out waiting for OpenVMM; output: {}",
                    output_tail(&self.output)
                );
            }
            match self.output_recv.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => self.output.extend_from_slice(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
        }
    }

    fn child_mut(&mut self) -> anyhow::Result<&mut Child> {
        self.child.as_mut().context("OpenVMM child is missing")
    }
}

impl Drop for OpenvmmTestProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct TcpConsole {
    stream: TcpStream,
    output_recv: mpsc::Receiver<Vec<u8>>,
    output: Vec<u8>,
}

impl TcpConsole {
    fn connect(address: SocketAddr) -> anyhow::Result<Self> {
        let started = Instant::now();
        let stream = loop {
            match TcpStream::connect_timeout(&address, Duration::from_millis(250)) {
                Ok(stream) => break stream,
                Err(error) if started.elapsed() < PHASE_2_TIMEOUT => {
                    let _ = error;
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to connect to microVM console at {address}")
                    });
                }
            }
        };
        stream.set_nodelay(true)?;
        let mut reader = stream.try_clone()?;
        let (output_send, output_recv) = mpsc::channel();
        thread::spawn(move || {
            let mut chunk = vec![0; 4096];
            while let Ok(count) = reader.read(&mut chunk) {
                if count == 0 || output_send.send(chunk[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            stream,
            output_recv,
            output: Vec::new(),
        })
    }

    fn send_line(&mut self, line: &str) -> anyhow::Result<()> {
        writeln!(self.stream, "{line}")?;
        self.stream.flush()?;
        Ok(())
    }

    fn send_bytes(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.stream.write_all(bytes)?;
        self.stream.flush()?;
        Ok(())
    }

    fn wait_for(&mut self, marker: &[u8]) -> anyhow::Result<()> {
        let started = Instant::now();
        while !contains_bytes(&self.output, marker) {
            let remaining = PHASE_2_TIMEOUT.saturating_sub(started.elapsed());
            anyhow::ensure!(
                !remaining.is_zero(),
                "timed out waiting for console marker {:?}; output: {}",
                String::from_utf8_lossy(marker),
                output_tail(&self.output)
            );
            match self
                .output_recv
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(chunk) => self.output.extend_from_slice(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!(
                        "console disconnected before marker {:?}; output: {}",
                        String::from_utf8_lossy(marker),
                        output_tail(&self.output)
                    )
                }
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        drop(self.stream);
        while let Ok(chunk) = self.output_recv.recv_timeout(Duration::from_millis(100)) {
            self.output.extend_from_slice(&chunk);
        }
        self.output
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn count_output_lines(output: &[u8], marker: &[u8]) -> usize {
    output
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| *line == marker)
        .count()
}

fn output_line_value<'a>(output: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    output
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .find_map(|line| line.strip_prefix(prefix))
}

fn output_tail(output: &[u8]) -> String {
    const MAX_OUTPUT: usize = 16 * 1024;
    String::from_utf8_lossy(&output[output.len().saturating_sub(MAX_OUTPUT)..]).into_owned()
}

fn phase_2_args(hypervisor: &str) -> Vec<OsString> {
    [
        "--single-process",
        "--machine",
        "microvm",
        "--hypervisor",
        hypervisor,
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

#[openvmm_test_no_agent(ignore(
    reason = "requires a published microVM PVH kernel and initramfs",
    microvm_pvh_x64
))]
async fn phase_1_lifecycle(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(30);
    const BOOT_MARKER: &str = "ALPINE-MICROVM-BOOT-OK";
    const CONTINUED_MARKER: &str = "PETRI-MICROVM-SNAPSHOT-CONTINUED";
    const RAW_MARKER: &[u8] = b"\0\r\n\x7f\xffPETRI-MICROVM-SNAPSHOT-CONTINUED";

    let mut vm = config.with_microvm_machine().run_without_agent().await?;

    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_output(BOOT_MARKER))
        .await
        .context("timed out waiting for microVM boot marker")??;

    let save_error = vm
        .backend()
        .save_state()
        .await
        .expect_err("microVM host save unexpectedly succeeded");
    assert!(
        format!("{save_error:#}").contains("save is unavailable for microVM ABI version 1"),
        "unexpected microVM host-save error: {save_error:#}"
    );
    let pulse_error = vm
        .backend()
        .pulse_save_restore()
        .await
        .expect_err("microVM pulse save/restore unexpectedly succeeded");
    assert!(
        format!("{pulse_error:#}")
            .contains("save and restore are unavailable for this machine profile"),
        "unexpected microVM pulse-save error: {pulse_error:#}"
    );

    vm.backend()
        .write_microvm_portb_input(
            format!(
                "printf '\\013' | dd of=/dev/port bs=1 seek=112 count=1 conv=notrunc 2>/dev/null; \
                 rtc=$(dd if=/dev/port bs=1 skip=113 count=1 2>/dev/null | od -An -tu1 | tr -d '[:space:]'); \
                 [ \"$rtc\" = 6 ] || {{ nvx-exit 40; exit; }}; \
                 [ \"$(date +%s)\" -ge 1500000000 ] || {{ nvx-exit 41; exit; }}; \
                 irq0=$(awk '/^[[:space:]]*0:/ {{ print $2; exit }}' /proc/interrupts); \
                 [ \"${{irq0:-0}}\" -gt 0 ] || {{ nvx-exit 42; exit; }}; \
                 up1=$(awk '{{ print int($1); exit }}' /proc/uptime); sleep 1; \
                 up2=$(awk '{{ print int($1); exit }}' /proc/uptime); \
                 [ $((up2 - up1)) -ge 1 ] || {{ nvx-exit 43; exit; }}; \
                 for octal in 000 015 012 177 377; do \
                     printf \"\\\\$octal\" | dd of=/dev/port bs=1 seek=233 count=1 conv=notrunc 2>/dev/null; \
                 done; \
                 nvx-snapshot; echo {CONTINUED_MARKER}; nvx-exit 37\n"
            )
            .as_bytes(),
        )
        .await?;

    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(RAW_MARKER))
        .await
        .context("microVM raw output or snapshot continuation marker was not observed")??;

    let halt = CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.wait_for_halt())
        .await
        .context("timed out waiting for microVM status shutdown")??;
    assert_eq!(halt.reason, PetriHaltReason::PowerOff);
    assert!(
        halt.detail.contains("code: 37"),
        "expected microVM shutdown status 37, got {}",
        halt.detail
    );

    vm.teardown().await
}

#[vmm_test_with(
    openvmm,
    noagent,
    requires(microvm_pvh),
    configs(microvm_pvh_x64[
        petri_artifacts_vmm_test::artifacts::OPENVMM_NATIVE
    ])
)]
async fn phase_2_snapshot_restore<OpenvmmArtifact>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: (petri::ResolvedArtifact<OpenvmmArtifact>,),
) -> anyhow::Result<()> {
    const MEMORY_BYTES: u64 = 128 * 1024 * 1024;
    const CONTINUED_MARKER: &[u8] = b"PHASE2-CONTINUED-ONCE";
    const NO_DESTINATION_MARKER: &[u8] = b"PHASE2-NO-DESTINATION-CONTINUED";

    let (openvmm,) = artifacts;
    let kernel = config
        .linux_direct_boot_files()
        .context("phase-2 test requires direct-boot Linux artifacts")?
        .0
        .to_path_buf();
    let reseed_helper = base64::engine::general_purpose::STANDARD
        .decode(include_str!("data/openvmm-reseed.b64").trim())
        .context("failed to decode the guest reseed helper")?;
    let initrd = config
        .prepare_initrd_with_file("openvmm-reseed", &reseed_helper, 0o100755)
        .context("failed to inject the guest reseed helper")?;
    let hypervisor = if cfg!(windows) {
        "whp"
    } else if cfg!(target_os = "linux") {
        "kvm"
    } else {
        anyhow::bail!("microVM phase-2 restore requires Windows/WHP or Linux/KVM");
    };
    let temp_dir = if cfg!(target_os = "linux") {
        tempfile::Builder::new()
            .prefix("openvmm-phase2-")
            .tempdir_in("/tmp")
    } else {
        tempfile::tempdir()
    }
    .context("failed to create phase-2 test directory")?;
    let snapshot_dir = temp_dir.path().join("snapshot");

    let mut no_destination = OpenvmmTestProcess::launch(openvmm.get(), &{
        let mut args = phase_2_args(hypervisor);
        args.extend([
            "--memory".into(),
            "128M".into(),
            "--kernel".into(),
            kernel.as_os_str().to_owned(),
            "--initrd".into(),
            initrd.as_os_str().to_owned(),
        ]);
        args
    })?;
    no_destination.wait_for(MICROVM_BOOT_MARKER)?;
    no_destination.send_line("nvx-snapshot; echo PHASE2-NO-DESTINATION-CONTINUED; nvx-exit 38")?;
    let (status, output) = no_destination.wait()?;
    anyhow::ensure!(
        status.code() == Some(38),
        "no-destination VM exited with {status}; output: {}",
        output_tail(&output)
    );
    anyhow::ensure!(
        count_output_lines(&output, NO_DESTINATION_MARKER) == 1,
        "no-destination continuation marker was not emitted exactly once"
    );

    let mut capture_args = phase_2_args(hypervisor);
    capture_args.extend([
        "--memory".into(),
        "128M".into(),
        "--kernel".into(),
        kernel.as_os_str().to_owned(),
        "--initrd".into(),
        initrd.as_os_str().to_owned(),
        "--snapshot-destination".into(),
        snapshot_dir.as_os_str().to_owned(),
    ]);
    let cold_start_started = Instant::now();
    let mut source = OpenvmmTestProcess::launch(openvmm.get(), &capture_args)?;
    source.wait_for(MICROVM_BOOT_MARKER)?;
    let cold_start = cold_start_started.elapsed();
    let expected_clocksource = if cfg!(target_os = "linux") {
        "kvm-clock"
    } else {
        "tsc"
    };
    let select_clocksource = if cfg!(target_os = "linux") {
        "clock_tries=0; \
         while ! grep -qw kvm-clock /sys/devices/system/clocksource/clocksource0/available_clocksource \
             && [ $clock_tries -lt 100 ]; do sleep 0.05; clock_tries=$((clock_tries+1)); done; \
         grep -qw kvm-clock /sys/devices/system/clocksource/clocksource0/available_clocksource \
             || { echo PHASE2-KVM-CLOCK-UNAVAILABLE; nvx-exit 46; exit; }; \
         echo kvm-clock > /sys/devices/system/clocksource/clocksource0/current_clocksource; "
    } else {
        ""
    };
    let capture_workload = [
        select_clocksource,
        "clock_tries=0; \
         while [ \"$(cat /sys/devices/system/clocksource/clocksource0/current_clocksource)\" != EXPECTED_CLOCKSOURCE ] \
             && [ $clock_tries -lt 100 ]; do sleep 0.05; clock_tries=$((clock_tries+1)); done; \
         clocksource=$(cat /sys/devices/system/clocksource/clocksource0/current_clocksource); \
         [ \"$clocksource\" = EXPECTED_CLOCKSOURCE ] || { echo PHASE2-CLOCKSOURCE-$clocksource; nvx-exit 46; exit; }; \
         wall_before=$(date +%s); uptime_before=$(cut -d. -f1 /proc/uptime); \
         sleep 5 & timer_pid=$!; sleep 1; nvx-snapshot; \
         echo PHASE2-CONTINUED-ONCE; \
         wall_restored=$(date +%s); uptime_restored=$(cut -d. -f1 /proc/uptime); \
         echo PHASE2-DOWNTIME-$((wall_restored-wall_before))-$((uptime_restored-uptime_before)); \
         wait $timer_pid; echo PHASE2-TIMER-DONE; \
         printf '\\245' | dd of=/dev/port bs=1 seek=234 count=1 conv=notrunc 2>/dev/null; \
         rm -f /tmp/phase2-entropy-packet /tmp/entropy; i=0; \
         while [ $i -lt 83 ]; do \
             dd if=/dev/port bs=1 skip=233 count=1 2>/dev/null >> /tmp/phase2-entropy-packet; \
             i=$((i+1)); \
         done; \
         head -c 18 /tmp/phase2-entropy-packet | grep -q OPENVMM_ENTROPY_V1 || { nvx-exit 44; exit; }; \
         tail -c 64 /tmp/phase2-entropy-packet > /tmp/entropy; \
         /openvmm-reseed || { nvx-exit 45; exit; }; \
         rng=$(head -c 32 /dev/urandom | sha256sum | cut -d' ' -f1); echo PHASE2-RNG-$rng; \
            dd if=/dev/zero of=/tmp/phase2-dirty bs=1M count=32 2>/dev/null; nvx-exit 37",
        ]
        .concat();
    source.send_line(&capture_workload.replace("EXPECTED_CLOCKSOURCE", expected_clocksource))?;
    let (status, source_output) = source.wait()?;
    anyhow::ensure!(
        status.success(),
        "snapshot source exited with {status}; output: {}",
        output_tail(&source_output)
    );
    anyhow::ensure!(
        count_output_lines(&source_output, CONTINUED_MARKER) == 0,
        "source executed past the snapshot boundary"
    );
    anyhow::ensure!(
        snapshot_dir.is_dir(),
        "snapshot directory was not published"
    );
    openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)
        .context("published snapshot failed verification")?;

    thread::sleep(Duration::from_secs(3));
    let mut rng_hashes = Vec::new();
    let mut restore_latencies = Vec::new();
    for restore_index in 0..2 {
        let mut restore_args = phase_2_args(hypervisor);
        restore_args.extend([
            "--restore-snapshot".into(),
            snapshot_dir.as_os_str().to_owned(),
            "--restore-entropy".into(),
        ]);
        let restore_started = Instant::now();
        let mut restore = OpenvmmTestProcess::launch(openvmm.get(), &restore_args)?;
        restore.wait_for_output_line(CONTINUED_MARKER)?;
        restore_latencies.push(restore_started.elapsed());
        restore.wait_for_output_line(b"PHASE2-TIMER-DONE")?;
        let timer_elapsed = restore_started.elapsed();
        let (status, output) = restore.wait()?;
        anyhow::ensure!(
            status.code() == Some(37),
            "restore {restore_index} exited with {status}; output: {}",
            output_tail(&output)
        );
        anyhow::ensure!(
            count_output_lines(&output, CONTINUED_MARKER) == 1,
            "restore {restore_index} did not continue exactly once after the snapshot OUT"
        );
        anyhow::ensure!(
            count_output_lines(&output, b"PHASE2-TIMER-DONE") == 1,
            "restore {restore_index} did not complete the armed timer"
        );
        anyhow::ensure!(
            timer_elapsed < Duration::from_millis(3500),
            "restore {restore_index} did not shorten the armed timer by host downtime: {timer_elapsed:?}; output: {}",
            output_tail(&output)
        );
        let downtime = std::str::from_utf8(
            output_line_value(&output, b"PHASE2-DOWNTIME-")
                .context("restored guest did not report its clock deltas")?,
        )?;
        let (wall_delta, uptime_delta) = downtime
            .split_once('-')
            .context("restored guest reported malformed clock deltas")?;
        let wall_delta = wall_delta.parse::<u64>()?;
        let uptime_delta = uptime_delta.parse::<u64>()?;
        anyhow::ensure!(
            wall_delta >= 3 && uptime_delta >= 3 && wall_delta.abs_diff(uptime_delta) <= 1,
            "restore {restore_index} clocks did not advance coherently: wall={wall_delta}s uptime={uptime_delta}s"
        );
        let rng_hash = output_line_value(&output, b"PHASE2-RNG-")
            .context("restored guest did not report an RNG digest")?;
        anyhow::ensure!(
            rng_hash.len() == 64 && rng_hash.iter().all(u8::is_ascii_hexdigit),
            "restore {restore_index} reported a malformed RNG digest"
        );
        rng_hashes.push(rng_hash.to_vec());
        openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)
            .with_context(|| format!("restore {restore_index} modified snapshot artifacts"))?;
    }
    anyhow::ensure!(
        rng_hashes[0] != rng_hashes[1],
        "fresh restore entropy did not make cloned guest RNG output diverge"
    );
    tracing::info!(
        cold_start_ms = cold_start.as_secs_f64() * 1000.0,
        restore_0_ms = restore_latencies[0].as_secs_f64() * 1000.0,
        restore_1_ms = restore_latencies[1].as_secs_f64() * 1000.0,
        "phase-2 process-to-guest latency"
    );

    Ok(())
}

#[vmm_test_with(
    openvmm,
    noagent,
    requires(microvm_pvh),
    configs(microvm_pvh_x64[
        petri_artifacts_vmm_test::artifacts::OPENVMM_NATIVE
    ])
)]
async fn phase_3_console_snapshot_restore<OpenvmmArtifact>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: (petri::ResolvedArtifact<OpenvmmArtifact>,),
) -> anyhow::Result<()> {
    const MEMORY_BYTES: u64 = 128 * 1024 * 1024;
    const SNAPSHOT_MARKER: &[u8] = b"PHASE3-SNAPSHOT-NOW";
    const RX_MARKER: &[u8] = b"PHASE3-RX-RESTORED";
    const DONE_MARKER: &[u8] = b"PHASE3-TX-DONE";
    const BINARY_MARKER: &[u8] = b"\0\r\n\x7f\xffPHASE3-BINARY";

    let (openvmm,) = artifacts;
    let (kernel, initrd) = config
        .linux_direct_boot_files()
        .context("phase-3 test requires direct-boot Linux artifacts")?;
    let hypervisor = if cfg!(windows) {
        "whp"
    } else if cfg!(target_os = "linux") {
        "kvm"
    } else {
        anyhow::bail!("microVM phase-3 restore requires Windows/WHP or Linux/KVM");
    };
    let temp_dir = if cfg!(target_os = "linux") {
        tempfile::Builder::new()
            .prefix("openvmm-phase3-")
            .tempdir_in("/tmp")
    } else {
        tempfile::tempdir()
    }
    .context("failed to create phase-3 test directory")?;
    let snapshot_dir = temp_dir.path().join("snapshot");
    let address = {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?
    };

    let mut capture_args = phase_2_args(hypervisor);
    capture_args.extend([
        "--memory".into(),
        "128M".into(),
        "--kernel".into(),
        kernel.as_os_str().to_owned(),
        "--initrd".into(),
        initrd.as_os_str().to_owned(),
        "--snapshot-destination".into(),
        snapshot_dir.as_os_str().to_owned(),
        "--virtio-console".into(),
        format!("listen=tcp:{address}").into(),
    ]);
    let source = OpenvmmTestProcess::launch(openvmm.get(), &capture_args)?;
    let mut source_console = TcpConsole::connect(address)?;
    source_console.wait_for(MICROVM_BOOT_MARKER)?;
    source_console.send_line(&format!(
        "set -eu; \
         grep -q 'console=hvc1' /proc/cmdline || {{ nvx-exit 50; exit; }}; \
         grep -q 'virtio_mmio.device=0x1000@0xd0002000:7' /proc/cmdline || {{ nvx-exit 51; exit; }}; \
         [ -e /sys/class/tty/hvc1 ] || {{ nvx-exit 52; exit; }}; \
         stty -F /dev/hvc1 raw -echo; \
         printf '\\000\\015\\012\\177\\377PHASE3-BINARY\\n'; \
         rm -f /tmp/phase3-tx-started /tmp/phase3-restored; \
         (i=0; while [ $i -lt {PHASE_3_TX_COUNT} ]; do printf 'PHASE3-TX-%05d\\n' \"$i\"; i=$((i+1)); if [ $i -eq 100 ]; then touch /tmp/phase3-tx-started; while [ ! -e /tmp/phase3-restored ]; do sleep 0.01; done; fi; done) & tx_pid=$!; \
         while [ ! -e /tmp/phase3-tx-started ]; do sleep 0.01; done; \
         echo PHASE3-SNAPSHOT-NOW; nvx-snapshot; \
            phase3_rx=$(dd bs=1 count=5 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
            [ \"$phase3_rx\" = 000d0a7fff ] || {{ nvx-exit 53; exit; }}; \
             touch /tmp/phase3-restored; \
            echo PHASE3-RX-RESTORED; \
         wait $tx_pid; echo PHASE3-TX-DONE; nvx-exit 37"
    ))?;
    source_console.wait_for(SNAPSHOT_MARKER)?;
    source_console.send_bytes(&[0, 13, 10, 127, 255])?;
    let (status, source_process_output) = source.wait()?;
    anyhow::ensure!(
        status.success(),
        "phase-3 source exited with {status}; process output: {}",
        output_tail(&source_process_output)
    );
    let source_console_output = source_console.finish();
    for index in 0..100 {
        let marker = format!("PHASE3-TX-{index:05}");
        anyhow::ensure!(
            count_output_lines(&source_console_output, marker.as_bytes()) == 1,
            "source did not forward the expected pre-snapshot TX prefix at record {index}"
        );
    }
    anyhow::ensure!(
        count_output_lines(&source_console_output, b"PHASE3-TX-00100") == 0,
        "source TX advanced past the deterministic snapshot boundary"
    );
    anyhow::ensure!(snapshot_dir.is_dir(), "phase-3 snapshot was not published");
    openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)?;

    for restore_index in 0..2 {
        let mut restore_args = phase_2_args(hypervisor);
        restore_args.extend([
            "--restore-snapshot".into(),
            snapshot_dir.as_os_str().to_owned(),
            "--restore-entropy".into(),
        ]);
        let restore = OpenvmmTestProcess::launch(openvmm.get(), &restore_args)?;
        let mut restore_console = TcpConsole::connect(address)?;
        restore_console.wait_for(RX_MARKER)?;
        restore_console.wait_for(DONE_MARKER)?;
        let (status, process_output) = restore.wait()?;
        anyhow::ensure!(
            status.code() == Some(37),
            "phase-3 restore {restore_index} exited with {status}; process output: {}",
            output_tail(&process_output)
        );
        let restore_output = restore_console.finish();
        let mut combined = source_console_output.clone();
        combined.extend_from_slice(&restore_output);
        anyhow::ensure!(
            contains_bytes(&combined, BINARY_MARKER),
            "phase-3 binary marker was lost across restore"
        );
        for index in 0..PHASE_3_TX_COUNT {
            let marker = format!("PHASE3-TX-{index:05}");
            anyhow::ensure!(
                count_output_lines(&combined, marker.as_bytes()) == 1,
                "phase-3 restore {restore_index} lost or duplicated TX record {index}; source tail: {}; restore tail: {}",
                output_tail(&source_console_output),
                output_tail(&restore_output)
            );
        }
        anyhow::ensure!(
            count_output_lines(&restore_output, RX_MARKER) == 1,
            "phase-3 restore {restore_index} did not preserve queued RX exactly once"
        );
        openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)
            .with_context(|| format!("phase-3 restore {restore_index} modified the snapshot"))?;
    }

    Ok(())
}

#[openvmm_test_no_agent(ignore(
    reason = "requires a published microVM PVH kernel and initramfs",
    microvm_pvh_x64
))]
async fn phase_1_block(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    use disk_backend_resources::LayeredDiskHandle;
    use disk_backend_resources::layer::RamDiskLayerHandle;
    use openvmm_defs::config::LoadMode;
    use openvmm_defs::config::VirtioBus;
    use openvmm_defs::config::append_microvm_virtio_discovery;
    use virtio_resources::blk::VirtioBlkHandle;

    const TIMEOUT: Duration = Duration::from_secs(30);
    const DISK_SIZE: u64 = 8 * 1024 * 1024;
    const WORKLOAD: &[u8] = br#"#!/bin/sh
set -eu
tries=0
while [ ! -b /dev/vda ] && [ "$tries" -lt 200 ]; do
    sleep 0.05
    tries=$((tries + 1))
done
device=$(readlink -f /sys/block/vda/device) || exit 20
grep -q 'virtio_mmio.device=0x1000@0xd0003000:4' /proc/cmdline || exit 21
[ "$(cat /sys/block/vda/size)" = 16384 ] || exit 23
printf MICROVM-BLOCK-RW-OK | dd of=/dev/vda bs=512 count=1 conv=sync,notrunc 2>/dev/null
readback=$(dd if=/dev/vda bs=512 count=1 2>/dev/null | head -c 15)
[ "$readback" = MICROVM-BLOCK-RW-OK ] || exit 24
exit 37
"#;

    let modified_initrd =
        config.prepare_initrd_with_file("microvm-block-test.sh", WORKLOAD, 0o100755)?;
    let disk = LayeredDiskHandle::single_layer(RamDiskLayerHandle {
        len: Some(DISK_SIZE),
        sector_size: None,
    })
    .into_resource();

    let mut vm = config
        .with_prebuilt_initrd(modified_initrd.to_path_buf())
        .with_microvm_machine()
        .modify_backend(move |backend| {
            backend.with_custom_config(|config| {
                let LoadMode::Pvh { cmdline, .. } = &mut config.load_mode else {
                    panic!("microVM test did not produce PVH load mode");
                };
                cmdline.push_str(" nvx_exec=/microvm-block-test.sh");
                append_microvm_virtio_discovery(cmdline, false, true).unwrap();
                config.virtio_devices.push((
                    VirtioBus::Mmio,
                    VirtioBlkHandle {
                        disk,
                        read_only: false,
                    }
                    .into_resource(),
                ));
            })
        })
        .run_without_agent()
        .await?;

    let halt = CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.wait_for_halt())
        .await
        .context("timed out waiting for microVM block workload")??;
    assert_eq!(halt.reason, PetriHaltReason::PowerOff);
    assert!(
        halt.detail.contains("code: 37"),
        "microVM block workload failed: {}",
        halt.detail
    );

    vm.teardown().await
}
