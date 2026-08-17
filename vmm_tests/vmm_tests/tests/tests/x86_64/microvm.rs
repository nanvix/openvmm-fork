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

#[vmm_test_with(
    openvmm,
    noagent,
    requires(microvm_pvh),
    configs(microvm_pvh_x64[
        petri_artifacts_vmm_test::artifacts::OPENVMM_NATIVE
    ])
)]
async fn phase_4_network_snapshot_restore<OpenvmmArtifact>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: (petri::ResolvedArtifact<OpenvmmArtifact>,),
) -> anyhow::Result<()> {
    const MEMORY_BYTES: u64 = 128 * 1024 * 1024;
    const BEFORE_MARKER: &[u8] = b"PHASE4-HTTP-BEFORE";
    const AFTER_MARKER: &[u8] = b"PHASE4-HTTP-AFTER";

    let (openvmm,) = artifacts;
    let (kernel, initrd) = config
        .linux_direct_boot_files()
        .context("phase-4 test requires direct-boot Linux artifacts")?;
    let hypervisor = if cfg!(windows) {
        "whp"
    } else if cfg!(target_os = "linux") {
        "kvm"
    } else {
        anyhow::bail!("microVM phase-4 restore requires Windows/WHP or Linux/KVM");
    };
    let temp_dir = if cfg!(target_os = "linux") {
        tempfile::Builder::new()
            .prefix("openvmm-phase4-")
            .tempdir_in("/tmp")
    } else {
        tempfile::tempdir()
    }
    .context("failed to create phase-4 test directory")?;
    let snapshot_dir = temp_dir.path().join("snapshot");

    let listener = TcpListener::bind("0.0.0.0:0")?;
    let http_port = listener.local_addr()?.port();
    let (request_send, request_recv) = mpsc::channel();
    let server = thread::spawn(move || -> anyhow::Result<()> {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(PHASE_2_TIMEOUT))?;
            let mut request = [0u8; 4096];
            let count = stream.read(&mut request)?;
            anyhow::ensure!(
                request[..count].starts_with(b"GET / HTTP/1."),
                "unexpected phase-4 HTTP request"
            );
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\nPHASE4-HTTP-OK",
            )?;
            stream.flush()?;
            request_send.send(()).ok();
        }
        Ok(())
    });

    let endpoint = format!("10.0.0.1:{http_port}");
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
        "--net".into(),
        "10.0.0.2/24".into(),
        "--allow-endpoint".into(),
        endpoint.clone().into(),
    ]);
    let mut source = OpenvmmTestProcess::launch(openvmm.get(), &capture_args)?;
    source.wait_for(MICROVM_BOOT_MARKER)?;
    source.send_line(&format!(
        "set -eu; \
         [ \"$(wget -qO- http://10.0.0.1:{http_port}/)\" = PHASE4-HTTP-OK ]; \
         echo PHASE4-HTTP-BEFORE; nvx-snapshot; \
         [ \"$(wget -qO- http://10.0.0.1:{http_port}/)\" = PHASE4-HTTP-OK ]; \
         echo PHASE4-HTTP-AFTER; nvx-exit 37"
    ))?;
    let (status, source_output) = source.wait()?;
    anyhow::ensure!(
        status.success(),
        "phase-4 snapshot source exited with {status}; output: {}",
        output_tail(&source_output)
    );
    anyhow::ensure!(
        count_output_lines(&source_output, BEFORE_MARKER) == 1
            && count_output_lines(&source_output, AFTER_MARKER) == 0,
        "phase-4 source crossed the snapshot boundary"
    );
    request_recv
        .recv_timeout(PHASE_2_TIMEOUT)
        .context("phase-4 pre-snapshot HTTP request was not observed")?;
    openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)?;

    let mut missing_policy_args = phase_2_args(hypervisor);
    missing_policy_args.extend([
        "--restore-snapshot".into(),
        snapshot_dir.as_os_str().to_owned(),
    ]);
    let missing_policy = OpenvmmTestProcess::launch(openvmm.get(), &missing_policy_args)?;
    let (status, output) = missing_policy.wait()?;
    anyhow::ensure!(
        !status.success() && contains_bytes(&output, b"restore-time egress policy does not match"),
        "phase-4 restore without policy was not rejected: {}",
        output_tail(&output)
    );

    for restore_index in 0..2 {
        let mut restore_args = phase_2_args(hypervisor);
        restore_args.extend([
            "--restore-snapshot".into(),
            snapshot_dir.as_os_str().to_owned(),
            "--restore-entropy".into(),
            "--allow-endpoint".into(),
            endpoint.clone().into(),
        ]);
        let mut restore = OpenvmmTestProcess::launch(openvmm.get(), &restore_args)?;
        restore.wait_for_output_line(AFTER_MARKER)?;
        let (status, output) = restore.wait()?;
        anyhow::ensure!(
            status.code() == Some(37),
            "phase-4 restore {restore_index} exited with {status}; output: {}",
            output_tail(&output)
        );
        anyhow::ensure!(
            count_output_lines(&output, AFTER_MARKER) == 1,
            "phase-4 restore {restore_index} did not complete HTTP exactly once"
        );
        request_recv
            .recv_timeout(PHASE_2_TIMEOUT)
            .with_context(|| {
                format!("phase-4 restore {restore_index} HTTP request was not observed")
            })?;
        openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)
            .with_context(|| format!("phase-4 restore {restore_index} modified the snapshot"))?;
    }
    server
        .join()
        .map_err(|_| anyhow::anyhow!("phase-4 HTTP server panicked"))??;
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
async fn phase_5_filesystem_snapshot_restore<OpenvmmArtifact>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: (petri::ResolvedArtifact<OpenvmmArtifact>,),
) -> anyhow::Result<()> {
    const MEMORY_BYTES: u64 = 128 * 1024 * 1024;
    const BEFORE_MARKER: &[u8] = b"PHASE5-FS-BEFORE";
    const AFTER_MARKER: &[u8] = b"PHASE5-FS-AFTER";

    let (openvmm,) = artifacts;
    let (kernel, initrd) = config
        .linux_direct_boot_files()
        .context("phase-5 test requires direct-boot Linux artifacts")?;
    let hypervisor = if cfg!(windows) {
        "whp"
    } else if cfg!(target_os = "linux") {
        "kvm"
    } else {
        anyhow::bail!("microVM phase-5 restore requires Windows/WHP or Linux/KVM");
    };
    let temp_dir = if cfg!(target_os = "linux") {
        tempfile::Builder::new()
            .prefix("openvmm-phase5-")
            .tempdir_in("/tmp")
    } else {
        tempfile::tempdir()
    }
    .context("failed to create phase-5 test directory")?;
    let snapshot_dir = temp_dir.path().join("snapshot");

    let read_only_root = temp_dir.path().join("read-only");
    fs_err::create_dir(&read_only_root)?;
    fs_err::write(read_only_root.join("seed"), b"PHASE5-READ-ONLY")?;
    let mut read_only_args = phase_2_args(hypervisor);
    read_only_args.extend([
        "--memory".into(),
        "128M".into(),
        "--kernel".into(),
        kernel.as_os_str().to_owned(),
        "--initrd".into(),
        initrd.as_os_str().to_owned(),
        "--mount".into(),
        format!("/mnt/share,{},ro", read_only_root.display()).into(),
    ]);
    let mut read_only = OpenvmmTestProcess::launch(openvmm.get(), &read_only_args)?;
    read_only.wait_for(MICROVM_BOOT_MARKER)?;
    read_only.send_line(
        "set -eu; mkdir -p /mnt/share; mount -t virtiofs microvm /mnt/share; \
         [ \"$(cat /mnt/share/seed)\" = PHASE5-READ-ONLY ]; \
         if touch /mnt/share/mutation 2>/dev/null; then nvx-exit 20; fi; \
         grep -q 'virtio_mmio.device=0x1000@0xd0001000:6' /proc/cmdline; \
         grep -q 'virtfs_tag=microvm' /proc/cmdline; \
         grep -q 'virtfs_mode=ro' /proc/cmdline; nvx-exit 38",
    )?;
    let (status, output) = read_only.wait()?;
    anyhow::ensure!(
        status.code() == Some(38) && !read_only_root.join("mutation").exists(),
        "phase-5 read-only enforcement failed with {status}: {}",
        output_tail(&output)
    );

    let root = temp_dir.path().join("live-root");
    fs_err::create_dir(&root)?;
    fs_err::create_dir(root.join("directory"))?;
    fs_err::write(root.join("directory").join("entry-a"), b"a")?;
    fs_err::write(root.join("directory").join("entry-b"), b"b")?;
    let long_name_suffix = "x".repeat(80);
    for index in 0..1000 {
        fs_err::write(
            root.join("directory")
                .join(format!("{index:04}-{long_name_suffix}")),
            b"",
        )?;
    }
    fs_err::write(root.join("open-handle"), b"")?;

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
        "--mount".into(),
        format!("/mnt/share,{},rw", root.display()).into(),
    ]);
    let mut source = OpenvmmTestProcess::launch(openvmm.get(), &capture_args)?;
    source.wait_for(MICROVM_BOOT_MARKER)?;
    source.send_line(
        "set -eu; mkdir -p /mnt/share; mount -t virtiofs microvm /mnt/share; \
         grep -q 'virtfs_dir=/mnt/share' /proc/cmdline; \
         grep -q 'virtfs_mode=rw' /proc/cmdline; \
         exec 3<>/mnt/share/open-handle; \
         mkfifo /tmp/phase5-directory; exec 5<>/tmp/phase5-directory; \
         find /mnt/share/directory -mindepth 1 -maxdepth 1 -type f >&5 & dir_pid=$!; \
         IFS= read -r first_entry <&5; sleep 1; \
         printf PHASE5-HANDLE-BEFORE >&3; \
         dd if=/dev/zero of=/mnt/share/active.bin bs=1M count=32 2>/dev/null & io_pid=$!; \
         echo PHASE5-FS-BEFORE; nvx-snapshot; wait \"$io_pid\"; \
         printf PHASE5-HANDLE-AFTER >&3; exec 3>&-; \
         [ \"$(wc -c </mnt/share/active.bin)\" = 33554432 ]; \
         [ \"$(cat /mnt/share/open-handle)\" = \
           PHASE5-HANDLE-BEFOREPHASE5-HANDLE-AFTER ]; \
         dir_count=1; while [ \"$dir_count\" -lt 1002 ]; do \
           IFS= read -r next_entry <&5; dir_count=$((dir_count + 1)); \
         done; wait \"$dir_pid\"; [ \"$dir_count\" = 1002 ]; \
         if IFS= read -r -t 1 unexpected_entry <&5; then nvx-exit 21; fi; exec 5>&-; \
         echo PHASE5-FS-AFTER; nvx-exit 37",
    )?;
    let (status, source_output) = source.wait()?;
    anyhow::ensure!(
        status.success(),
        "phase-5 snapshot source exited with {status}; output: {}",
        output_tail(&source_output)
    );
    anyhow::ensure!(
        count_output_lines(&source_output, BEFORE_MARKER) == 1
            && count_output_lines(&source_output, AFTER_MARKER) == 0,
        "phase-5 source crossed the snapshot boundary"
    );
    anyhow::ensure!(
        fs_err::read(root.join("open-handle"))? == b"PHASE5-HANDLE-BEFORE",
        "phase-5 source completed a post-snapshot filesystem write"
    );
    openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)?;
    fs_err::write(root.join("directory").join("late-entry"), b"late")?;

    let mut missing_mount_args = phase_2_args(hypervisor);
    missing_mount_args.extend([
        "--restore-snapshot".into(),
        snapshot_dir.as_os_str().to_owned(),
    ]);
    let missing_mount = OpenvmmTestProcess::launch(openvmm.get(), &missing_mount_args)?;
    let (status, output) = missing_mount.wait()?;
    anyhow::ensure!(
        !status.success() && contains_bytes(&output, b"requires a fresh --mount attachment"),
        "phase-5 restore without a mount was not rejected: {}",
        output_tail(&output)
    );

    let replacement_root = temp_dir.path().join("replacement-root");
    fs_err::create_dir(&replacement_root)?;
    let mut replacement_args = phase_2_args(hypervisor);
    replacement_args.extend([
        "--restore-snapshot".into(),
        snapshot_dir.as_os_str().to_owned(),
        "--mount".into(),
        format!("/mnt/share,{},rw", replacement_root.display()).into(),
    ]);
    let replacement = OpenvmmTestProcess::launch(openvmm.get(), &replacement_args)?;
    let (status, output) = replacement.wait()?;
    anyhow::ensure!(
        !status.success() && contains_bytes(&output, b"root identity does not match"),
        "phase-5 restore accepted a replacement root: {}",
        output_tail(&output)
    );

    let original = root.join("open-handle");
    let saved_original = root.join("saved-open-handle");
    fs_err::rename(&original, &saved_original)?;
    fs_err::write(&original, b"replacement")?;
    let mut replaced_object_args = phase_2_args(hypervisor);
    replaced_object_args.extend([
        "--restore-snapshot".into(),
        snapshot_dir.as_os_str().to_owned(),
        "--mount".into(),
        format!("/mnt/share,{},rw", root.display()).into(),
    ]);
    let replaced_object = OpenvmmTestProcess::launch(openvmm.get(), &replaced_object_args)?;
    let (status, output) = replaced_object.wait()?;
    anyhow::ensure!(
        !status.success(),
        "phase-5 restore accepted a replaced open object: {}",
        output_tail(&output)
    );
    fs_err::remove_file(&original)?;
    fs_err::rename(&saved_original, &original)?;

    for restore_index in 0..2 {
        let mut restore_args = phase_2_args(hypervisor);
        restore_args.extend([
            "--restore-snapshot".into(),
            snapshot_dir.as_os_str().to_owned(),
            "--restore-entropy".into(),
            "--mount".into(),
            format!("/mnt/share,{},rw", root.display()).into(),
        ]);
        let mut restore = OpenvmmTestProcess::launch(openvmm.get(), &restore_args)?;
        restore.wait_for_output_line(AFTER_MARKER)?;
        let (status, output) = restore.wait()?;
        anyhow::ensure!(
            status.code() == Some(37),
            "phase-5 restore {restore_index} exited with {status}: {}",
            output_tail(&output)
        );
        anyhow::ensure!(
            count_output_lines(&output, AFTER_MARKER) == 1,
            "phase-5 restore {restore_index} did not complete exactly once"
        );
        openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)
            .with_context(|| format!("phase-5 restore {restore_index} modified the snapshot"))?;
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
                append_microvm_virtio_discovery(cmdline, None, None, false, true).unwrap();
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

#[vmm_test_with(openvmm, noagent, requires(microvm_pvh), configs(microvm_pvh_x64))]
async fn phase_4_virtio_net(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    use net_backend_resources::consomme::ConsommeHandle;
    use net_backend_resources::consomme::StaticIpv4Config;
    use net_backend_resources::egress::EgressPolicy;
    use net_backend_resources::egress::EgressPolicyMode;
    use openvmm_defs::config::LoadMode;
    use openvmm_defs::config::MicrovmNetworkConfig;
    use openvmm_defs::config::VirtioBus;
    use openvmm_defs::config::append_microvm_virtio_discovery;
    use openvmm_defs::config::microvm_virtio_net_irq;
    use virtio_resources::net::VirtioNetHandle;

    const TIMEOUT: Duration = Duration::from_secs(30);
    const WORKLOAD: &[u8] = br#"#!/bin/sh
set -eu
tries=0
device=
while [ -z "$device" ] && [ "$tries" -lt 200 ]; do
    for path in /sys/class/net/*; do
        [ -e "$path" ] || continue
        candidate=${path##*/}
        [ "$candidate" = lo ] && continue
        device=$candidate
        break
    done
    [ -n "$device" ] || sleep 0.05
    tries=$((tries + 1))
done
[ -n "$device" ] || exit 20
grep -q 'virtio_mmio.device=0x1000@0xd0000000:' /proc/cmdline || exit 21
grep -q 'virtnet_ip=10.0.0.2' /proc/cmdline || exit 22
grep -q 'virtnet_mask=255.255.255.0' /proc/cmdline || exit 23
grep -q 'virtnet_gw=10.0.0.1' /proc/cmdline || exit 24
[ "$(cat /sys/class/net/$device/address)" = '52:54:00:00:00:02' ] || exit 25
grep -qi 'd0000000-d0000fff.*virtio' /proc/iomem || exit 26
ifconfig "$device" 10.0.0.2 netmask 255.255.255.0 up || exit 27
route add default gw 10.0.0.1 dev "$device" 2>/dev/null || true
ping -c 1 -W 5 10.0.0.1 >/dev/null || exit 28
exit 37
"#;

    let modified_initrd =
        config.prepare_initrd_with_file("microvm-net-test.sh", WORKLOAD, 0o100755)?;
    let network: MicrovmNetworkConfig = "10.0.0.2/24".parse()?;
    let static_ipv4 = StaticIpv4Config {
        guest_ipv4: network.guest_ipv4,
        prefix_length: network.prefix_length,
        gateway_ipv4: network.derived_gateway_ipv4,
        gateway_mac: network.gateway_mac,
    };
    let endpoint = ConsommeHandle {
        cidr: None,
        ports: Vec::new(),
        recv: None,
        static_ipv4: Some(static_ipv4.clone()),
    }
    .into_resource();
    let policy = EgressPolicy::new(
        network.guest_ipv4,
        network.derived_gateway_ipv4,
        EgressPolicyMode::AllowAll,
    );
    let irq = microvm_virtio_net_irq(None)?;

    let mut vm = config
        .with_prebuilt_initrd(modified_initrd.to_path_buf())
        .with_microvm_machine()
        .modify_backend(move |backend| {
            backend.with_custom_config(|config| {
                let LoadMode::Pvh { cmdline, .. } = &mut config.load_mode else {
                    panic!("microVM test did not produce PVH load mode");
                };
                cmdline.push_str(" nvx_exec=/microvm-net-test.sh");
                append_microvm_virtio_discovery(
                    cmdline,
                    Some((&network, irq, cfg!(windows))),
                    None,
                    false,
                    false,
                )
                .unwrap();
                config.microvm_network = Some(network.clone());
                config.virtio_devices.push((
                    VirtioBus::Mmio,
                    VirtioNetHandle {
                        max_queues: Some(1),
                        mac_address: network.guest_mac,
                        endpoint,
                        egress_policy: Some(policy),
                        save_restore: true,
                        static_ipv4: Some(static_ipv4),
                        effective_features: Some(openvmm_defs::config::MICROVM_VIRTIO_NET_FEATURES),
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
        .context("timed out waiting for microVM network workload")??;
    assert_eq!(halt.reason, PetriHaltReason::PowerOff);
    assert!(
        halt.detail.contains("code: 37"),
        "microVM network workload failed: {}",
        halt.detail
    );
    vm.teardown().await
}
