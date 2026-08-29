// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use base64::Engine;
use mesh::CancelContext;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use std::ffi::OsString;
use std::hash::Hasher;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::net::UdpSocket;
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
const MICROVM_SHELL_PROMPT: &[u8] = b"/ # ";
const PHASE_2_TIMEOUT: Duration = Duration::from_secs(60);
const PHASE_3_TX_COUNT: usize = 10_000;

fn snapshot_payload_fingerprint(snapshot_dir: &Path) -> anyhow::Result<(u64, u64, Option<u64>)> {
    fn file_fingerprint(path: &Path) -> anyhow::Result<u64> {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("failed to open snapshot payload {}", path.display()))?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut buffer = vec![0; 1024 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .with_context(|| format!("failed to read snapshot payload {}", path.display()))?;
            if count == 0 {
                break;
            }
            hasher.write(&buffer[..count]);
        }
        Ok(hasher.finish())
    }

    Ok((
        file_fingerprint(&snapshot_dir.join("state.bin"))?,
        file_fingerprint(&snapshot_dir.join("memory.bin"))?,
        snapshot_dir
            .join(openvmm_helpers::snapshot::SCRATCH_FILE_NAME)
            .is_file()
            .then(|| {
                file_fingerprint(&snapshot_dir.join(openvmm_helpers::snapshot::SCRATCH_FILE_NAME))
            })
            .transpose()?,
    ))
}

fn microvm_hypervisor() -> anyhow::Result<&'static str> {
    if cfg!(windows) {
        Ok("whp")
    } else if cfg!(target_os = "linux") {
        Ok(if Path::new("/dev/mshv").exists() {
            "mshv"
        } else {
            "kvm"
        })
    } else {
        anyhow::bail!("microVM tests require Windows/WHP or Linux/KVM/MSHV")
    }
}

struct OpenvmmTestProcess {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    output_recv: mpsc::Receiver<Vec<u8>>,
    output: Vec<u8>,
}

impl OpenvmmTestProcess {
    fn launch(executable: &Path, args: &[OsString]) -> anyhow::Result<Self> {
        let openvmm_log = std::env::var_os("VMM_TEST_OPENVMM_LOG").unwrap_or_else(|| "off".into());
        let mut child = Command::new(executable)
            .args(args)
            .env("OPENVMM_LOG", openvmm_log)
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

    fn drain_output(&mut self) {
        while let Ok(chunk) = self.output_recv.try_recv() {
            self.output.extend_from_slice(&chunk);
        }
    }

    fn failure_context(&mut self) -> String {
        self.drain_output();
        let (status, exited) = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => (format!("exited with {status}"), true),
                Ok(None) => ("was still running".to_owned(), false),
                Err(error) => (format!("status query failed: {error}"), false),
            },
            None => ("was already reaped".to_owned(), true),
        };
        if exited {
            while let Ok(chunk) = self.output_recv.recv_timeout(Duration::from_millis(100)) {
                self.output.extend_from_slice(&chunk);
            }
        } else {
            self.drain_output();
        }
        format!("{status}; output: {}", output_tail(&self.output))
    }

    fn wait_for(&mut self, marker: &[u8]) -> anyhow::Result<()> {
        let started = Instant::now();
        loop {
            self.drain_output();
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
                    self.drain_output();
                    if !contains_bytes(&self.output, marker) {
                        anyhow::bail!(
                            "OpenVMM output closed before guest marker; output: {}",
                            output_tail(&self.output)
                        );
                    }
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
            if remaining.is_zero() {
                anyhow::bail!(
                    "timed out waiting for guest output line {:?}; output: {}",
                    String::from_utf8_lossy(marker),
                    output_tail(&self.output)
                );
            }
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

    fn wait_for_output_line(&mut self, marker: &[u8]) -> anyhow::Result<()> {
        let started = Instant::now();
        while count_output_lines(&self.output, marker) == 0 {
            let remaining = PHASE_2_TIMEOUT.saturating_sub(started.elapsed());
            anyhow::ensure!(
                !remaining.is_zero(),
                "timed out waiting for console output line {:?}; output: {}",
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
                        "console disconnected before output line {:?}; output: {}",
                        String::from_utf8_lossy(marker),
                        output_tail(&self.output)
                    )
                }
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        self.stream.shutdown(std::net::Shutdown::Both).ok();
        drop(self.stream);
        while let Ok(chunk) = self.output_recv.recv() {
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
        .split_inclusive(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_suffix(b"\n"))
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

async fn microvm_v3_smp(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    processor_count: u32,
) -> anyhow::Result<()> {
    use openvmm_defs::config::LoadMode;

    const TIMEOUT: Duration = Duration::from_secs(60);
    let workload = format!(
        r#"#!/bin/sh
set -eu
fail() {{ echo SMP-PROBE-FAIL code=$1; nvx-exit "$1"; exit 1; }}
expected={processor_count}
online=$(getconf _NPROCESSORS_ONLN) || fail 20
[ "$online" -eq "$expected" ] || fail 21
loc_before=$(awk -v expected="$expected" '/^LOC:/ {{ for (cpu = 0; cpu < expected; cpu++) printf "%s%s", $(cpu + 2), (cpu + 1 == expected ? "" : " "); exit }}' /proc/interrupts)
[ "$(printf "%s\n" "$loc_before" | awk '{{ print NF }}')" -eq "$expected" ] || fail 31
uptime_before=$(awk '{{ print int($1 * 100); exit }}' /proc/uptime)
cpu=0
workers=0
while [ "$cpu" -lt "$expected" ]; do
        topology=/sys/devices/system/cpu/cpu${{cpu}}/topology
        [ "$(cat "$topology/physical_package_id")" -eq 0 ] || fail 22
        [ "$(cat "$topology/die_id")" -eq 0 ] || fail 23
        [ "$(cat "$topology/core_id")" -eq "$cpu" ] || fail 24
        [ "$(cat "$topology/thread_siblings_list")" = "$cpu" ] || fail 25
        apic_id=$(awk -v target="$cpu" '$1 == "processor" {{ processor = $3 }} $1 == "apicid" && processor == target {{ print $3; exit }}' /proc/cpuinfo) || fail 26
        [ "$apic_id" -eq "$cpu" ] || fail 27
        actual=$(taskset -c "$cpu" sh -c 'awk "{{print \$39}}" /proc/self/stat') || fail 28
        [ "$actual" -eq "$cpu" ] || fail 29
        taskset -c "$cpu" sleep 0.1 &
        echo SMP-WORKER-OK cpu=$cpu apic=$apic_id
        workers=$((workers + 1))
        cpu=$((cpu + 1))
done
wait
loc_after=$(awk -v expected="$expected" '/^LOC:/ {{ for (cpu = 0; cpu < expected; cpu++) printf "%s%s", $(cpu + 2), (cpu + 1 == expected ? "" : " "); exit }}' /proc/interrupts)
cpu=0
while [ "$cpu" -lt "$expected" ]; do
    field=$((cpu + 1))
    before=$(printf "%s\n" "$loc_before" | awk -v field="$field" '{{ print $field }}')
    after=$(printf "%s\n" "$loc_after" | awk -v field="$field" '{{ print $field }}')
    [ "$after" -gt "$before" ] || fail 32
    if [ "$cpu" -gt 0 ]; then
        ipi=$(awk -v field=$((cpu + 2)) '/^(RES|CAL):/ {{ total += $field }} END {{ print total + 0 }}' /proc/interrupts)
        [ "$ipi" -gt 0 ] || fail 33
    fi
    cpu=$((cpu + 1))
done
uptime_after=$(awk '{{ print int($1 * 100); exit }}' /proc/uptime)
[ "$uptime_after" -gt "$uptime_before" ] || fail 34
[ "$workers" -eq "$expected" ] || fail 30
echo SMP-INTERRUPTS-OK loc_before=$loc_before loc_after=$loc_after
echo SMP-TOPOLOGY-OK requested=$expected online=$online sockets=1 cores=$expected threads=1 bsp=0 workers=$workers
echo NVX-SMP-PROBE-OK
nvx-exit 37
"#
    );
    let modified_initrd =
        config.prepare_initrd_with_file("microvm-v3-smp-test.sh", workload.as_bytes(), 0o100755)?;
    let mut vm = config
        .with_prebuilt_initrd(modified_initrd.to_path_buf())
        .with_microvm_v3_machine(processor_count)
        .modify_backend(|backend| {
            backend.with_custom_config(|config| {
                let LoadMode::Pvh { cmdline, .. } = &mut config.load_mode else {
                    panic!("microVM v3 SMP test did not produce PVH load mode");
                };
                cmdline.push_str(" nvx_exec=/microvm-v3-smp-test.sh");
            })
        })
        .run_without_agent()
        .await?;

    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(
            vm.backend()
                .wait_for_microvm_portb_output("NVX-SMP-PROBE-OK"),
        )
        .await
        .context("timed out waiting for microVM v3 SMP probe marker")??;
    let halt = CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.wait_for_halt())
        .await
        .context("timed out waiting for microVM v3 SMP shutdown")??;
    assert_eq!(halt.reason, PetriHaltReason::PowerOff);
    assert!(
        halt.detail.contains("code: 37"),
        "microVM v3 SMP workload failed: {}",
        halt.detail
    );
    vm.teardown().await
}

#[openvmm_test_no_agent(microvm_pvh_x64)]
async fn microvm_v3_smp_1(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    microvm_v3_smp(config, 1).await
}

#[openvmm_test_no_agent(microvm_pvh_x64)]
async fn microvm_v3_smp_2(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    microvm_v3_smp(config, 2).await
}

#[openvmm_test_no_agent(microvm_pvh_x64)]
async fn microvm_v3_smp_4(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    microvm_v3_smp(config, 4).await
}

#[openvmm_test_no_agent(microvm_pvh_x64)]
async fn microvm_v3_smp_8(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    microvm_v3_smp(config, 8).await
}

#[vmm_test_with(
    openvmm,
    noagent,
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
    const TIMER_WAIT_PREFIX: &[u8] = b"PHASE2-TIMER-WAIT-";
    const UPTIME_CENTISECONDS_PREFIX: &[u8] = b"PHASE2-UPTIME-CS-";
    const MIN_CLOCK_ADVANCE_SECS: u64 = 4;
    const MAX_CLOCK_SKEW_SECS: u64 = 1;

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
    let hypervisor = microvm_hypervisor()?;
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
    no_destination.send_line("nvx-snapshot; echo PHASE2-NO-DESTINATION-CONTINUED; nvx-exit 38")?;
    no_destination.wait_for(MICROVM_BOOT_MARKER)?;
    no_destination.wait_for(MICROVM_SHELL_PROMPT)?;
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
    let (select_clocksource, validate_clocksource) = match hypervisor {
        "kvm" => (
            "clock_tries=0; \
             while ! grep -qw kvm-clock /sys/devices/system/clocksource/clocksource0/available_clocksource \
                 && [ $clock_tries -lt 100 ]; do sleep 0.05; clock_tries=$((clock_tries+1)); done; \
             grep -qw kvm-clock /sys/devices/system/clocksource/clocksource0/available_clocksource \
                 || { echo PHASE2-KVM-CLOCK-UNAVAILABLE; nvx-exit 46; exit; }; \
             echo kvm-clock > /sys/devices/system/clocksource/clocksource0/current_clocksource; ",
            "[ \"$(cat /sys/devices/system/clocksource/clocksource0/current_clocksource)\" = kvm-clock ] \
                 || { nvx-exit 46; exit; }; ",
        ),
        "whp" => (
            "",
            "[ \"$(cat /sys/devices/system/clocksource/clocksource0/current_clocksource)\" = tsc ] \
                 || { nvx-exit 46; exit; }; ",
        ),
        "mshv" => ("", ""),
        _ => unreachable!(),
    };
    // Measure a separate sleeping task so command processing by this shell is
    // not mistaken for CPU time accumulated during snapshot downtime.
    let arm_timer = "\
        sleep 3600 & p=$!; \
        sleep 5 & timer_pid=$!; \
        sleep 1; \
        [ -r /proc/$p/stat ] || { nvx-exit 47; exit; }; \
        ";
    let complete_timer = "\
        wait $timer_pid; \
        timer_wait_after=$(cut -d. -f1 /proc/uptime); \
        echo PHASE2-CONTINUED-ONCE; \
        echo PHASE2-DOWNTIME-$((wall_restored-wall_before))-$((uptime_restored-uptime_before)); \
        echo PHASE2-UPTIME-CS-$((uptime_restored_cs-uptime_before_cs)); \
        echo PHASE2-CPU-$((process_cpu_restored-process_cpu_before))-$((thread_cpu_restored-thread_cpu_before)); \
        echo PHASE2-TIMER-WAIT-$((timer_wait_after-timer_wait_before)); \
        echo PHASE2-TIMER-DONE; \
        ";
    let capture_workload = [
        select_clocksource,
        "\n",
        validate_clocksource,
        "\n",
        arm_timer,
        "\n",
        "wall_before=$(date +%s); uptime_before_raw=$(cut -d' ' -f1 /proc/uptime); \
         uptime_before=${uptime_before_raw%%.*}; uptime_before_cs=$(echo $uptime_before_raw | tr -d .); \
         process_cpu_before=$(awk '{print $14+$15}' /proc/$p/stat); \
         thread_cpu_before=$(awk '{print $14+$15}' /proc/$p/task/$p/stat)\n",
        "nvx-snapshot\n",
        "wall_restored=$(date +%s); uptime_restored_raw=$(cut -d' ' -f1 /proc/uptime); \
         uptime_restored=${uptime_restored_raw%%.*}; uptime_restored_cs=$(echo $uptime_restored_raw | tr -d .); \
         process_cpu_restored=$(awk '{print $14+$15}' /proc/$p/stat); \
         thread_cpu_restored=$(awk '{print $14+$15}' /proc/$p/task/$p/stat); \
         kill $p; \
         timer_wait_before=$(cut -d. -f1 /proc/uptime)\n",
        complete_timer,
        "\n",
        "printf '\\245' | dd of=/dev/port bs=1 seek=234 count=1 conv=notrunc 2>/dev/null\n",
        "rm -f /tmp/phase2-entropy-packet /tmp/entropy; i=0\n",
        "while [ $i -lt 83 ]; do \
             dd if=/dev/port bs=1 skip=233 count=1 2>/dev/null >> /tmp/phase2-entropy-packet; \
             i=$((i+1)); \
         done\n",
        "head -c 18 /tmp/phase2-entropy-packet | grep -q OPENVMM_ENTROPY_V1 \
             || { nvx-exit 44; exit; }\n",
        "tail -c 64 /tmp/phase2-entropy-packet > /tmp/entropy\n",
        "/openvmm-reseed || { nvx-exit 45; exit; }\n",
        "rng=$(head -c 32 /dev/urandom | sha256sum | cut -d' ' -f1); echo PHASE2-RNG-$rng\n",
        "dd if=/dev/zero of=/tmp/phase2-dirty bs=1M count=32 2>/dev/null; nvx-exit 37\n",
    ]
    .concat();
    source.send_line("cat >/tmp/phase2-workload <<'PHASE2_WORKLOAD'")?;
    source.send_line(&capture_workload)?;
    source.send_line("PHASE2_WORKLOAD")?;
    source.send_line("sh /tmp/phase2-workload")?;
    source.wait_for(MICROVM_BOOT_MARKER)?;
    source.wait_for(MICROVM_SHELL_PROMPT)?;
    let cold_start = cold_start_started.elapsed();
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
    let (manifest, _) = openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)
        .context("published snapshot failed structural validation")?;
    anyhow::ensure!(
        manifest.version == openvmm_helpers::snapshot::MANIFEST_VERSION
            && manifest.state_sha256.is_empty()
            && manifest.memory_sha256.is_empty(),
        "new microVM snapshot contains legacy artifact digests"
    );
    let snapshot_capture_time: std::time::SystemTime = manifest
        .machine_contract
        .as_ref()
        .context("published snapshot is missing its machine contract")?
        .capture_wall_clock
        .try_into()
        .context("published snapshot has an invalid capture wall clock")?;
    let snapshot_fingerprint = snapshot_payload_fingerprint(&snapshot_dir)?;

    let mut rng_hashes = Vec::new();
    let mut restore_latencies = Vec::new();
    for restore_index in 0..2 {
        thread::sleep(Duration::from_secs(5));
        let minimum_downtime = std::time::SystemTime::now()
            .duration_since(snapshot_capture_time)
            .context("host wall clock moved before snapshot capture time")?;
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
        let timer_wait = std::str::from_utf8(
            output_line_value(&output, TIMER_WAIT_PREFIX)
                .context("restored guest did not report its timer wait")?,
        )?
        .parse::<u64>()?;
        anyhow::ensure!(
            timer_wait <= 1,
            "restore {restore_index} did not shorten the armed timer by host downtime: guest waited {timer_wait}s; output: {}",
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
        // Whole-second samples can straddle different boundaries. Require one
        // clock to cover the minimum interval while keeping both coherent.
        anyhow::ensure!(
            wall_delta.max(uptime_delta) >= MIN_CLOCK_ADVANCE_SECS
                && wall_delta.abs_diff(uptime_delta) <= MAX_CLOCK_SKEW_SECS,
            "restore {restore_index} clocks did not reflect host downtime coherently: wall={wall_delta}s uptime={uptime_delta}s; expected either clock to advance by at least {MIN_CLOCK_ADVANCE_SECS}s with at most {MAX_CLOCK_SKEW_SECS}s skew"
        );
        let uptime_centiseconds = std::str::from_utf8(
            output_line_value(&output, UPTIME_CENTISECONDS_PREFIX)
                .context("restored guest did not report precise uptime advancement")?,
        )?
        .parse::<u128>()?;
        let minimum_downtime_centiseconds = minimum_downtime.as_millis() / 10;
        anyhow::ensure!(
            uptime_centiseconds + 5 >= minimum_downtime_centiseconds,
            "restore {restore_index} discarded captured guest uptime: advanced {uptime_centiseconds}cs, expected at least {minimum_downtime_centiseconds}cs from snapshot capture to restore launch"
        );
        let cpu = std::str::from_utf8(
            output_line_value(&output, b"PHASE2-CPU-")
                .context("restored guest did not report its CPU clock deltas")?,
        )?;
        let (process_cpu_delta, thread_cpu_delta) = cpu
            .split_once('-')
            .context("restored guest reported malformed CPU clock deltas")?;
        let process_cpu_delta = process_cpu_delta.parse::<u64>()?;
        let thread_cpu_delta = thread_cpu_delta.parse::<u64>()?;
        anyhow::ensure!(
            process_cpu_delta == 0 && thread_cpu_delta == 0,
            "restore {restore_index} sleeping-task CPU clocks advanced during downtime: process={process_cpu_delta} ticks thread={thread_cpu_delta} ticks"
        );
        let rng_hash = output_line_value(&output, b"PHASE2-RNG-")
            .context("restored guest did not report an RNG digest")?;
        anyhow::ensure!(
            rng_hash.len() == 64 && rng_hash.iter().all(u8::is_ascii_hexdigit),
            "restore {restore_index} reported a malformed RNG digest"
        );
        rng_hashes.push(rng_hash.to_vec());
        anyhow::ensure!(
            snapshot_payload_fingerprint(&snapshot_dir)? == snapshot_fingerprint,
            "restore {restore_index} modified snapshot payloads"
        );
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
    let hypervisor = microvm_hypervisor()?;
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
    source_console.wait_for(MICROVM_SHELL_PROMPT)?;
    let tx_count = match hypervisor {
        "mshv" => 100,
        "whp" => 1_000,
        _ => PHASE_3_TX_COUNT,
    };
    let completion = if hypervisor == "mshv" {
        "nvx-exit 37"
    } else {
        "wait $tx_pid; echo PHASE3-TX-DONE; nvx-exit 37"
    };
    let restored_marker = if hypervisor == "mshv" {
        ""
    } else {
        "echo PHASE3-RX-RESTORED;"
    };
    // Avoid tty-special bytes so this checks queued binary RX, not line discipline.
    let receive = if hypervisor == "mshv" {
        "IFS= read -r phase3_rx; \
         [ \"$phase3_rx\" = PHASE3-RX ] || { nvx-exit 53; exit; };"
    } else {
        "phase3_rx=$(dd bs=1 count=5 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
         [ \"$phase3_rx\" = 0001027fff ] || { nvx-exit 53; exit; };"
    };
    source_console.send_line(&format!(
        "set -eu; \
         grep -q 'console=hvc1' /proc/cmdline || {{ nvx-exit 50; exit; }}; \
         grep -q 'virtio_mmio.device=0x1000@0xd0002000:7' /proc/cmdline || {{ nvx-exit 51; exit; }}; \
         [ -e /sys/class/tty/hvc1 ] || {{ nvx-exit 52; exit; }}; \
         stty -F /dev/hvc1 raw -echo; \
         printf '\\000\\015\\012\\177\\377PHASE3-BINARY\\n'; \
         rm -f /tmp/phase3-tx-started /tmp/phase3-resume; \
         mkfifo /tmp/phase3-resume; \
         (i=0; while [ $i -lt {tx_count} ]; do printf 'PHASE3-TX-%05d\\n' \"$i\"; i=$((i+1)); if [ $i -eq 100 ]; then touch /tmp/phase3-tx-started; IFS= read -r phase3_resume < /tmp/phase3-resume; [ \"$phase3_resume\" = resume ]; fi; done) & tx_pid=$!; \
         while [ ! -e /tmp/phase3-tx-started ]; do sleep 0.01; done; \
         echo PHASE3-SNAPSHOT-NOW; sleep 1; nvx-snapshot; \
            {receive} \
             printf 'resume\\n' > /tmp/phase3-resume; \
            {restored_marker} \
         {completion}"
    ))?;
    source_console.wait_for(SNAPSHOT_MARKER)?;
    if hypervisor == "mshv" {
        source_console.send_bytes(b"PHASE3-RX\n")?;
    } else {
        source_console.send_bytes(&[0, 1, 2, 127, 255])?;
    }
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
    let snapshot_fingerprint = snapshot_payload_fingerprint(&snapshot_dir)?;

    for restore_index in 0..2 {
        let mut restore_args = phase_2_args(hypervisor);
        restore_args.extend([
            "--restore-snapshot".into(),
            snapshot_dir.as_os_str().to_owned(),
            "--restore-entropy".into(),
        ]);
        let mut restore = OpenvmmTestProcess::launch(openvmm.get(), &restore_args)?;
        let mut restore_console = TcpConsole::connect(address)?;
        if hypervisor != "mshv" {
            if let Err(error) = restore_console.wait_for(RX_MARKER) {
                let process_context = restore.failure_context();
                return Err(error).with_context(|| {
                    format!("phase-3 restore {restore_index}: OpenVMM {process_context}")
                });
            }
            if let Err(error) = restore_console.wait_for(DONE_MARKER) {
                let process_context = restore.failure_context();
                return Err(error).with_context(|| {
                    format!("phase-3 restore {restore_index}: OpenVMM {process_context}")
                });
            }
        }
        let (status, process_output) = restore.wait()?;
        // On MSHV, exit 37 is the signal that the restored RX line was
        // consumed and validated; console TX is too slow to be the signal.
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
        for index in 0..tx_count {
            let marker = format!("PHASE3-TX-{index:05}");
            anyhow::ensure!(
                count_output_lines(&combined, marker.as_bytes()) == 1,
                "phase-3 restore {restore_index} lost or duplicated TX record {index}; source tail: {}; restore tail: {}",
                output_tail(&source_console_output),
                output_tail(&restore_output)
            );
        }
        if hypervisor != "mshv" {
            anyhow::ensure!(
                count_output_lines(&restore_output, RX_MARKER) == 1,
                "phase-3 restore {restore_index} did not preserve queued RX exactly once"
            );
        }
        anyhow::ensure!(
            snapshot_payload_fingerprint(&snapshot_dir)? == snapshot_fingerprint,
            "phase-3 restore {restore_index} modified snapshot payloads"
        );
    }

    Ok(())
}

#[vmm_test_with(
    openvmm,
    noagent,
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
    const OLD_FLOW_INVALIDATED_MARKER: &[u8] = b"PHASE4-OLD-FLOW-INVALIDATED";
    const AFTER_MARKER: &[u8] = b"PHASE4-HTTP-AFTER";

    let (openvmm,) = artifacts;
    let (kernel, initrd) = config
        .linux_direct_boot_files()
        .context("phase-4 test requires direct-boot Linux artifacts")?;
    let hypervisor = microvm_hypervisor()?;
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
    let udp_socket = UdpSocket::bind("127.0.0.1:0")?;
    udp_socket.set_read_timeout(Some(PHASE_2_TIMEOUT))?;
    let udp_port = udp_socket.local_addr()?.port();
    let (event_send, event_recv) = mpsc::channel();
    let server = thread::spawn(move || -> anyhow::Result<()> {
        let (mut stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(PHASE_2_TIMEOUT))?;
        let mut request = [0u8; 4096];
        let count = stream.read(&mut request)?;
        anyhow::ensure!(
            request[..count].starts_with(b"GET /hold HTTP/1."),
            "unexpected phase-4 held HTTP request"
        );
        event_send.send("held").ok();
        match stream.read(&mut request) {
            Ok(0) => {}
            Ok(_) => anyhow::bail!("the pre-capture HTTP request was replayed"),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                anyhow::bail!("the pre-capture HTTP connection remained live after capture")
            }
            Err(error) => return Err(error.into()),
        }
        event_send.send("old-flow-closed").ok();

        for _ in 0..2 {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(PHASE_2_TIMEOUT))?;
            let count = stream.read(&mut request)?;
            anyhow::ensure!(
                request[..count].starts_with(b"GET /fresh HTTP/1."),
                "a restored pre-capture request was replayed instead of opening a fresh flow"
            );
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\nPHASE4-HTTP-OK",
            )?;
            stream.flush()?;
            event_send.send("fresh").ok();
        }
        Ok(())
    });

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
        "--network-profile".into(),
        "portable".into(),
        "--allow-host".into(),
        "10.0.0.1".into(),
    ]);
    let mut source = OpenvmmTestProcess::launch(openvmm.get(), &capture_args)?;
    // The guest image has no deterministic raw DNS client, and its resolver
    // follows the host's live DNS policy. This native test checks DNS bootstrap;
    // Consomme's UDP and TCP DNS transaction tests cover resolver behavior.
    // Egress denial is covered precisely by net_backend_resources unit tests.
    source.send_line(&format!(
        "set -eu; \
         fail() {{ nvx-exit \"$1\"; exit 1; }}; \
         grep -q 'virtnet_dns=10.0.0.1' /proc/cmdline || fail 20; \
         ping -c 1 -W 5 10.0.0.1 >/dev/null || fail 21; \
         printf PHASE4-UDP | nc -u -w 5 10.0.0.1 {udp_port} || fail 22; \
         if ping -c 1 -W 1 -s 2000 10.0.0.1 >/dev/null; then fail 23; fi; \
         wget -T 5 -qO /tmp/phase4-held http://10.0.0.1:{http_port}/hold & held=$!; \
         sleep 1; \
         echo PHASE4-HTTP-BEFORE; nvx-snapshot; \
         if wait \"$held\"; then fail 24; fi; \
         echo PHASE4-OLD-FLOW-INVALIDATED; \
         [ \"$(wget -qO- http://10.0.0.1:{http_port}/fresh)\" = PHASE4-HTTP-OK ] || fail 25; \
         echo PHASE4-HTTP-AFTER; nvx-exit 37"
    ))?;
    source.wait_for(MICROVM_BOOT_MARKER)?;
    source.wait_for(MICROVM_SHELL_PROMPT)?;
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
    let event = event_recv
        .recv_timeout(PHASE_2_TIMEOUT)
        .context("phase-4 pre-snapshot HTTP request was not observed")?;
    anyhow::ensure!(event == "held", "unexpected phase-4 event {event}");
    let event = event_recv
        .recv_timeout(PHASE_2_TIMEOUT)
        .context("phase-4 pre-snapshot HTTP flow did not close at capture")?;
    anyhow::ensure!(
        event == "old-flow-closed",
        "unexpected phase-4 event {event}"
    );
    let mut udp_payload = [0u8; 64];
    let (udp_count, _) = udp_socket
        .recv_from(&mut udp_payload)
        .context("phase-4 guest UDP datagram was not observed")?;
    anyhow::ensure!(
        &udp_payload[..udp_count] == b"PHASE4-UDP",
        "unexpected phase-4 guest UDP payload"
    );
    openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)?;
    let snapshot_fingerprint = snapshot_payload_fingerprint(&snapshot_dir)?;

    let mut missing_policy_args = phase_2_args(hypervisor);
    missing_policy_args.extend([
        "--restore-snapshot".into(),
        snapshot_dir.as_os_str().to_owned(),
        "--network-profile".into(),
        "portable".into(),
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
            "--network-profile".into(),
            "portable".into(),
            "--restore-entropy".into(),
            "--allow-host".into(),
            "10.0.0.1".into(),
        ]);
        let mut restore = OpenvmmTestProcess::launch(openvmm.get(), &restore_args)?;
        restore.wait_for_output_line(OLD_FLOW_INVALIDATED_MARKER)?;
        restore.wait_for_output_line(AFTER_MARKER)?;
        let (status, output) = restore.wait()?;
        anyhow::ensure!(
            status.code() == Some(37),
            "phase-4 restore {restore_index} exited with {status}; output: {}",
            output_tail(&output)
        );
        anyhow::ensure!(
            count_output_lines(&output, OLD_FLOW_INVALIDATED_MARKER) == 1
                && count_output_lines(&output, AFTER_MARKER) == 1,
            "phase-4 restore {restore_index} did not invalidate the old flow and complete one fresh HTTP request"
        );
        let event = event_recv.recv_timeout(PHASE_2_TIMEOUT).with_context(|| {
            format!("phase-4 restore {restore_index} fresh HTTP request was not observed")
        })?;
        anyhow::ensure!(event == "fresh", "unexpected phase-4 event {event}");
        anyhow::ensure!(
            snapshot_payload_fingerprint(&snapshot_dir)? == snapshot_fingerprint,
            "phase-4 restore {restore_index} modified snapshot payloads"
        );
    }
    server
        .join()
        .map_err(|_| anyhow::anyhow!("phase-4 HTTP server panicked"))??;
    Ok(())
}

#[vmm_test_with(
    openvmm,
    noagent,
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
    let hypervisor = microvm_hypervisor()?;
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
    read_only.send_line(
        "set -eu; grep -q ' /mnt/share virtiofs ' /proc/mounts; \
         [ \"$(cat /mnt/share/seed)\" = PHASE5-READ-ONLY ]; \
         if touch /mnt/share/mutation 2>/dev/null; then nvx-exit 20; fi; \
         grep -q 'virtio_mmio.device=0x1000@0xd0001000:6' /proc/cmdline; \
         grep -q 'virtfs_tag=microvm' /proc/cmdline; \
         grep -q 'virtfs_mode=ro' /proc/cmdline; nvx-exit 38",
    )?;
    read_only.wait_for(MICROVM_BOOT_MARKER)?;
    read_only.wait_for(MICROVM_SHELL_PROMPT)?;
    let (status, output) = read_only.wait()?;
    anyhow::ensure!(
        status.code() == Some(38) && !read_only_root.join("mutation").exists(),
        "phase-5 read-only enforcement failed with {status}: {}",
        output_tail(&output)
    );

    let root = temp_dir.path().join("live-root");
    fs_err::create_dir(&root)?;
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
    source.send_line(
        "set -eu; grep -q ' /mnt/share virtiofs ' /proc/mounts; \
         grep -q 'virtfs_dir=/mnt/share' /proc/cmdline; \
         grep -q 'virtfs_mode=rw' /proc/cmdline; \
         exec 3<>/mnt/share/open-handle; \
         printf PHASE5-HANDLE-BEFORE >&3; \
         echo PHASE5-FS-BEFORE; nvx-snapshot; \
         printf PHASE5-HANDLE-AFTER >&3; exec 3>&-; \
         [ \"$(cat /mnt/share/open-handle)\" = \
           PHASE5-HANDLE-BEFOREPHASE5-HANDLE-AFTER ]; \
         echo PHASE5-FS-AFTER; nvx-exit 37",
    )?;
    source.wait_for(MICROVM_BOOT_MARKER)?;
    source.wait_for(MICROVM_SHELL_PROMPT)?;
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
    let snapshot_fingerprint = snapshot_payload_fingerprint(&snapshot_dir)?;

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
        let restore = OpenvmmTestProcess::launch(openvmm.get(), &restore_args)?;
        let (status, output) = restore.wait()?;
        anyhow::ensure!(
            status.code() == Some(37),
            "phase-5 restore {restore_index} exited with {status}: {}",
            output_tail(&output)
        );
        anyhow::ensure!(
            snapshot_payload_fingerprint(&snapshot_dir)? == snapshot_fingerprint,
            "phase-5 restore {restore_index} modified snapshot payloads"
        );
    }

    Ok(())
}

#[vmm_test_with(
    openvmm,
    noagent,
    configs(microvm_pvh_x64[
        petri_artifacts_vmm_test::artifacts::OPENVMM_NATIVE
    ])
)]
async fn microvm_v2_paired_scratch_snapshot_restore<OpenvmmArtifact>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: (petri::ResolvedArtifact<OpenvmmArtifact>,),
) -> anyhow::Result<()> {
    const MEMORY_BYTES: u64 = 128 * 1024 * 1024;
    const LAYER_BYTES: usize = 1024 * 1024;
    const SCRATCH_BYTES: usize = 8 * 1024 * 1024;
    const POST_OUT_MARKER: &[u8] = b"MICROVM-V2-SCRATCH-POST-OUT";
    const RESTORED_MARKER: &[u8] = b"MICROVM-V2-SCRATCH-RESTORED";

    let (openvmm,) = artifacts;
    let (kernel, initrd) = config
        .linux_direct_boot_files()
        .context("ABI-v2 snapshot test requires direct-boot Linux artifacts")?;
    let hypervisor = microvm_hypervisor()?;
    let temp_dir = if cfg!(target_os = "linux") {
        tempfile::Builder::new()
            .prefix("openvmm-microvm-v2-snapshot-")
            .tempdir_in("/tmp")
    } else {
        tempfile::tempdir()
    }
    .context("failed to create ABI-v2 snapshot test directory")?;
    let snapshot_dir = temp_dir.path().join("snapshot");
    let layer_path = temp_dir.path().join("distro.erofs");
    let wrong_layer_path = temp_dir.path().join("wrong-distro.erofs");
    let scratch_path = temp_dir.path().join("source-scratch.raw");
    std::fs::write(&layer_path, vec![0x3c; LAYER_BYTES])?;
    std::fs::write(&wrong_layer_path, vec![0xc3; LAYER_BYTES])?;
    std::fs::write(&scratch_path, vec![0xa5; SCRATCH_BYTES])?;

    let block_arg = |role: &str, path: &Path, read_only: bool| -> OsString {
        format!(
            "{role}:file:{}{}",
            path.display(),
            if read_only { ",ro" } else { "" }
        )
        .into()
    };
    let mut capture_args = [
        "--single-process",
        "--machine",
        "microvm-v2",
        "--hypervisor",
        hypervisor,
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    capture_args.extend([
        "--memory".into(),
        "128M".into(),
        "--kernel".into(),
        kernel.as_os_str().to_owned(),
        "--initrd".into(),
        initrd.as_os_str().to_owned(),
        "--snapshot-destination".into(),
        snapshot_dir.as_os_str().to_owned(),
        "--snapshot-tier".into(),
        "workload-start".into(),
        "--microvm-sandbox-block".into(),
        block_arg("distro", &layer_path, true),
        "--microvm-sandbox-block".into(),
        format!("scratch:delay:250:file:{}", scratch_path.display()).into(),
    ]);

    let mut source = OpenvmmTestProcess::launch(openvmm.get(), &capture_args)?;
    source.wait_for(MICROVM_BOOT_MARKER)?;
    source.wait_for(MICROVM_SHELL_PROMPT)?;
    source.send_line(
        "set -eu; \
         tries=0; while { [ ! -b /dev/vda ] || [ ! -b /dev/vdb ]; } && [ $tries -lt 600 ]; do sleep 0.05; tries=$((tries+1)); done; \
         [ \"$(cat /sys/block/vda/ro)\" = 1 ] || { nvx-exit 60; exit; }; \
         [ \"$(cat /sys/block/vdb/ro)\" = 0 ] || { nvx-exit 61; exit; }; \
         dd if=/dev/zero of=/dev/vdb bs=512 count=1 conv=notrunc 2>/dev/null & writer=$!; \
         tries=0; while [ $tries -lt 2000 ]; do set -- $(cat /sys/block/vdb/inflight); [ $(($1+$2)) -gt 0 ] && break; kill -0 $writer 2>/dev/null || break; tries=$((tries+1)); done; \
         [ $tries -lt 2000 ] && kill -0 $writer 2>/dev/null || { nvx-exit 62; exit; }; \
         printf '\\001' | dd of=/dev/port bs=1 seek=1541 count=1 conv=notrunc 2>/dev/null; \
         printf '\002' | dd of=/dev/port bs=1 seek=1541 count=1 conv=notrunc 2>/dev/null; \
         echo MICROVM-V2-SCRATCH-POST-OUT; \
         wait $writer; \
         first_byte=$(dd if=/dev/vdb bs=1 count=1 2>/dev/null | od -An -tu1 | tr -d '[:space:]'); \
         [ \"$first_byte\" = 0 ] || { nvx-exit 63; exit; }; \
         echo MICROVM-V2-SCRATCH-RESTORED; \
         printf PRIVATE-RESTORE-MUTATION | dd of=/dev/vdb bs=512 count=1 conv=sync,notrunc 2>/dev/null; \
         sync; nvx-exit 37",
    )?;
    let (status, output) = source.wait()?;
    anyhow::ensure!(
        status.success(),
        "ABI-v2 snapshot source exited with {status}; output: {}",
        output_tail(&output)
    );
    anyhow::ensure!(
        count_output_lines(&output, POST_OUT_MARKER) == 0
            && count_output_lines(&output, RESTORED_MARKER) == 0,
        "ABI-v2 source continued past its snapshot boundary"
    );

    let (manifest, _) = openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)?;
    let contract = manifest
        .machine_contract
        .as_ref()
        .context("ABI-v2 snapshot is missing its machine contract")?;
    anyhow::ensure!(
        contract.microvm_abi_version == openvmm_defs::config::MICROVM_ABI_VERSION_2
            && contract.microvm_sandbox_blocks.last().is_some_and(|block| {
                block.role == "scratch"
                    && block.artifact == openvmm_helpers::snapshot::SCRATCH_FILE_NAME
            }),
        "ABI-v2 snapshot did not publish a paired scratch contract"
    );
    let snapshot_fingerprint = snapshot_payload_fingerprint(&snapshot_dir)?;

    let restore_args = |layer: &Path| {
        let mut args = [
            "--single-process",
            "--machine",
            "microvm-v2",
            "--hypervisor",
            hypervisor,
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        args.extend([
            "--restore-snapshot".into(),
            snapshot_dir.as_os_str().to_owned(),
            "--restore-entropy".into(),
            "--microvm-sandbox-block".into(),
            block_arg("distro", layer, true),
        ]);
        args
    };
    for restore_index in 0..2 {
        let restore = OpenvmmTestProcess::launch(openvmm.get(), &restore_args(&layer_path))?;
        let (status, output) = restore.wait()?;
        anyhow::ensure!(
            status.code() == Some(37),
            "ABI-v2 restore {restore_index} exited with {status}; output: {}",
            output_tail(&output)
        );
        anyhow::ensure!(
            count_output_lines(&output, POST_OUT_MARKER) == 1
                && count_output_lines(&output, RESTORED_MARKER) == 1,
            "ABI-v2 restore {restore_index} did not observe one coherent scratch outcome"
        );
        anyhow::ensure!(
            snapshot_payload_fingerprint(&snapshot_dir)? == snapshot_fingerprint,
            "ABI-v2 restore {restore_index} modified snapshot artifacts"
        );
    }

    let rejected_restore = |description: &str, layer: &Path| -> anyhow::Result<()> {
        let restore = OpenvmmTestProcess::launch(openvmm.get(), &restore_args(layer))?;
        let (status, output) = restore.wait()?;
        anyhow::ensure!(
            !status.success()
                && count_output_lines(&output, POST_OUT_MARKER) == 0
                && count_output_lines(&output, RESTORED_MARKER) == 0,
            "{description} unexpectedly entered the guest; status={status}; output: {}",
            output_tail(&output)
        );
        Ok(())
    };
    rejected_restore("mismatched read-only layer", &wrong_layer_path)?;

    let published_scratch = snapshot_dir.join(openvmm_helpers::snapshot::SCRATCH_FILE_NAME);
    let scratch_bytes = std::fs::read(&published_scratch)?;
    std::fs::remove_file(&published_scratch)?;
    rejected_restore("missing paired scratch", &layer_path)?;
    std::fs::write(&published_scratch, &scratch_bytes)?;

    let mut corrupt = scratch_bytes.clone();
    corrupt[0] ^= 0xff;
    std::fs::write(&published_scratch, &corrupt)?;
    rejected_restore("corrupt paired scratch", &layer_path)?;
    std::fs::write(&published_scratch, &scratch_bytes)?;

    let scratch_file = std::fs::OpenOptions::new()
        .write(true)
        .open(&published_scratch)?;
    scratch_file.set_len((SCRATCH_BYTES - 512) as u64)?;
    drop(scratch_file);
    rejected_restore("truncated paired scratch", &layer_path)?;
    std::fs::write(&published_scratch, &scratch_bytes)?;

    Ok(())
}

#[vmm_test_with(
    openvmm,
    noagent,
    configs(microvm_pvh_x64[
        petri_artifacts_vmm_test::artifacts::OPENVMM_NATIVE
    ])
)]
async fn microvm_v2_fresh_scratch_snapshot_restore<OpenvmmArtifact>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: (petri::ResolvedArtifact<OpenvmmArtifact>,),
) -> anyhow::Result<()> {
    const MEMORY_BYTES: u64 = 128 * 1024 * 1024;
    const DISK_BYTES: usize = 1024 * 1024;
    const POST_OUT_MARKER: &[u8] = b"MICROVM-V2-FRESH-POST-OUT";

    let (openvmm,) = artifacts;
    let (kernel, initrd) = config
        .linux_direct_boot_files()
        .context("ABI-v2 fresh-scratch test requires direct-boot Linux artifacts")?;
    let hypervisor = microvm_hypervisor()?;
    let temp_dir = if cfg!(target_os = "linux") {
        tempfile::Builder::new()
            .prefix("openvmm-microvm-v2-fresh-")
            .tempdir_in("/tmp")
    } else {
        tempfile::tempdir()
    }
    .context("failed to create fresh-scratch test directory")?;
    let snapshot_dir = temp_dir.path().join("snapshot");
    let layer_path = temp_dir.path().join("distro.erofs");
    let capture_scratch_path = temp_dir.path().join("capture-scratch.raw");
    std::fs::write(&layer_path, vec![0x3c; DISK_BYTES])?;
    std::fs::write(&capture_scratch_path, vec![0xa5; DISK_BYTES])?;

    let block_arg = |role: &str, path: &Path, read_only: bool| -> OsString {
        format!(
            "{role}:file:{}{}",
            path.display(),
            if read_only { ",ro" } else { "" }
        )
        .into()
    };
    let base_args = || {
        [
            "--single-process",
            "--machine",
            "microvm-v2",
            "--hypervisor",
            hypervisor,
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>()
    };
    let mut capture_args = base_args();
    capture_args.extend([
        "--memory".into(),
        "128M".into(),
        "--kernel".into(),
        kernel.as_os_str().to_owned(),
        "--initrd".into(),
        initrd.as_os_str().to_owned(),
        "--snapshot-destination".into(),
        snapshot_dir.as_os_str().to_owned(),
        "--snapshot-tier".into(),
        "platform".into(),
        "--microvm-sandbox-block".into(),
        block_arg("distro", &layer_path, true),
        "--microvm-sandbox-block".into(),
        block_arg("scratch", &capture_scratch_path, false),
    ]);

    let mut source = OpenvmmTestProcess::launch(openvmm.get(), &capture_args)?;
    source.wait_for(MICROVM_BOOT_MARKER)?;
    source.wait_for(MICROVM_SHELL_PROMPT)?;
    source.send_line(
        "set -eu; \
         tries=0; while [ ! -b /dev/vdb ] && [ $tries -lt 600 ]; do sleep 0.05; tries=$((tries+1)); done; \
         printf '\\000' | dd of=/dev/port bs=1 seek=1541 count=1 conv=notrunc 2>/dev/null; \
         printf '\002' | dd of=/dev/port bs=1 seek=1541 count=1 conv=notrunc 2>/dev/null; \
         echo MICROVM-V2-FRESH-POST-OUT; \
         blockdev --flushbufs /dev/vdb; \
         value=$(dd if=/dev/vdb bs=1 count=1 2>/dev/null | od -An -tu1 | tr -d '[:space:]'); \
         echo MICROVM-V2-FRESH-SCRATCH-$value; nvx-exit 37",
    )?;
    let (status, output) = source.wait()?;
    anyhow::ensure!(
        status.success()
            && count_output_lines(&output, POST_OUT_MARKER) == 0
            && count_output_lines(&output, b"MICROVM-V2-FRESH-SCRATCH-165") == 0,
        "fresh-scratch source crossed its capture boundary: {}",
        output_tail(&output)
    );

    let (manifest, _) = openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)?;
    let scratch = manifest
        .machine_contract
        .as_ref()
        .and_then(|contract| contract.microvm_sandbox_blocks.last())
        .context("fresh-scratch snapshot is missing its scratch contract")?;
    anyhow::ensure!(
        scratch.role == "scratch"
            && scratch.identity_kind == "fresh"
            && scratch.identity.is_empty()
            && scratch.artifact.is_empty()
            && !snapshot_dir
                .join(openvmm_helpers::snapshot::SCRATCH_FILE_NAME)
                .exists(),
        "fresh-scratch snapshot unexpectedly contains paired state"
    );
    let snapshot_fingerprint = snapshot_payload_fingerprint(&snapshot_dir)?;

    let restore_args = |scratch_path: Option<&Path>| {
        let mut args = base_args();
        args.extend([
            "--restore-snapshot".into(),
            snapshot_dir.as_os_str().to_owned(),
            "--restore-entropy".into(),
            "--microvm-sandbox-block".into(),
            block_arg("distro", &layer_path, true),
        ]);
        if let Some(scratch_path) = scratch_path {
            args.extend([
                "--microvm-sandbox-block".into(),
                block_arg("scratch", scratch_path, false),
            ]);
        }
        args
    };
    for (restore_index, value) in [17_u8, 34].into_iter().enumerate() {
        let scratch_path = temp_dir
            .path()
            .join(format!("fresh-scratch-{restore_index}.raw"));
        std::fs::write(&scratch_path, vec![value; DISK_BYTES])?;
        let restore =
            OpenvmmTestProcess::launch(openvmm.get(), &restore_args(Some(&scratch_path)))?;
        let (status, output) = restore.wait()?;
        let marker = format!("MICROVM-V2-FRESH-SCRATCH-{value}");
        anyhow::ensure!(
            status.code() == Some(37)
                && count_output_lines(&output, POST_OUT_MARKER) == 1
                && count_output_lines(&output, marker.as_bytes()) == 1,
            "fresh-scratch restore {restore_index} did not use its new scratch; status={status}; output: {}",
            output_tail(&output)
        );
        anyhow::ensure!(
            snapshot_payload_fingerprint(&snapshot_dir)? == snapshot_fingerprint,
            "fresh-scratch restore {restore_index} modified snapshot artifacts"
        );
    }

    let missing = OpenvmmTestProcess::launch(openvmm.get(), &restore_args(None))?;
    let (status, output) = missing.wait()?;
    anyhow::ensure!(
        !status.success()
            && count_output_lines(&output, POST_OUT_MARKER) == 0
            && count_output_lines(&output, b"MICROVM-V2-FRESH-SCRATCH-165") == 0,
        "fresh-scratch restore without scratch entered the guest"
    );

    let wrong_geometry = temp_dir.path().join("wrong-geometry.raw");
    std::fs::write(&wrong_geometry, vec![0_u8; DISK_BYTES / 2])?;
    let wrong = OpenvmmTestProcess::launch(openvmm.get(), &restore_args(Some(&wrong_geometry)))?;
    let (status, output) = wrong.wait()?;
    anyhow::ensure!(
        !status.success()
            && count_output_lines(&output, POST_OUT_MARKER) == 0
            && count_output_lines(&output, b"MICROVM-V2-FRESH-SCRATCH-165") == 0,
        "fresh-scratch restore with wrong geometry entered the guest"
    );

    Ok(())
}

#[vmm_test_with(
    openvmm,
    noagent,
    configs(microvm_pvh_x64[
        petri_artifacts_vmm_test::artifacts::OPENVMM_NATIVE
    ])
)]
async fn microvm_v2_snapshot_tiers_and_restore_gate<OpenvmmArtifact>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    artifacts: (petri::ResolvedArtifact<OpenvmmArtifact>,),
) -> anyhow::Result<()> {
    const MEMORY_BYTES: u64 = 128 * 1024 * 1024;
    const DISK_BYTES: usize = 1024 * 1024;

    let (openvmm,) = artifacts;
    let (kernel, initrd) = config
        .linux_direct_boot_files()
        .context("snapshot tier test requires direct-boot Linux artifacts")?;
    let hypervisor = microvm_hypervisor()?;
    let temp_dir = if cfg!(target_os = "linux") {
        tempfile::Builder::new()
            .prefix("openvmm-microvm-v2-tiers-")
            .tempdir_in("/tmp")
    } else {
        tempfile::tempdir()
    }
    .context("failed to create snapshot tier test directory")?;
    let source_layer = temp_dir.path().join("source.erofs");
    let replacement_layer = temp_dir.path().join("replacement.erofs");
    std::fs::write(&source_layer, vec![0x3c; DISK_BYTES])?;
    std::fs::write(&replacement_layer, vec![0xc3; DISK_BYTES])?;

    let block_arg = |role: &str, path: &Path, read_only: bool| -> OsString {
        format!(
            "{role}:file:{}{}",
            path.display(),
            if read_only { ",ro" } else { "" }
        )
        .into()
    };
    let base_args = || {
        [
            "--single-process",
            "--machine",
            "microvm-v2",
            "--hypervisor",
            hypervisor,
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>()
    };

    for (tier, restore_policy, consumed_sections, single_use) in [
        (
            openvmm_helpers::snapshot::SNAPSHOT_TIER_PLATFORM,
            openvmm_helpers::snapshot::SNAPSHOT_RESTORE_POLICY_CLONE,
            openvmm_helpers::snapshot::SNAPSHOT_CONFIG_INVARIANTS,
            false,
        ),
        (
            openvmm_helpers::snapshot::SNAPSHOT_TIER_WORKLOAD_START,
            openvmm_helpers::snapshot::SNAPSHOT_RESTORE_POLICY_CLONE,
            openvmm_helpers::snapshot::SNAPSHOT_CONFIG_INVARIANTS
                | openvmm_helpers::snapshot::SNAPSHOT_CONFIG_IMAGE_BINDING
                | openvmm_helpers::snapshot::SNAPSHOT_CONFIG_SANDBOX,
            false,
        ),
        (
            openvmm_helpers::snapshot::SNAPSHOT_TIER_INSTANCE_CHECKPOINT,
            openvmm_helpers::snapshot::SNAPSHOT_RESTORE_POLICY_RESUME,
            openvmm_helpers::snapshot::SNAPSHOT_CONFIG_INVARIANTS
                | openvmm_helpers::snapshot::SNAPSHOT_CONFIG_IMAGE_BINDING
                | openvmm_helpers::snapshot::SNAPSHOT_CONFIG_SANDBOX,
            true,
        ),
    ] {
        let snapshot_dir = temp_dir.path().join(format!("{tier}-snapshot"));
        let source_scratch = temp_dir.path().join(format!("{tier}-source-scratch.raw"));
        let restore_scratch = temp_dir.path().join(format!("{tier}-restore-scratch.raw"));
        std::fs::write(&source_scratch, vec![0xa5; DISK_BYTES])?;
        std::fs::write(&restore_scratch, vec![0x5a; DISK_BYTES])?;
        let address = {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            listener.local_addr()?
        };
        let capture_marker = format!("TIER-{tier}-CAPTURE");
        let repair_marker = format!("TIER-{tier}-REPAIR");
        let input_marker = format!("TIER-{tier}-INPUT");
        let released_marker = format!("TIER-{tier}-RELEASED");
        let premature_marker = format!("TIER-{tier}-PREMATURE-INPUT");
        let mismatch_marker = format!("TIER-{tier}-MISMATCH-REJECTED");
        let setup_marker = format!("TIER-{tier}-SETUP-READY");
        let hook_marker = format!("TIER-{tier}-HOOK-READY");
        let workload_marker = format!("TIER-{tier}-WORKLOAD-RAN");
        let layer_marker = format!("TIER-{tier}-LAYER-");
        let platform_tier = tier == openvmm_helpers::snapshot::SNAPSHOT_TIER_PLATFORM;
        let mismatched_tier = if platform_tier {
            openvmm_helpers::snapshot::SNAPSHOT_TIER_WORKLOAD_START
        } else {
            openvmm_helpers::snapshot::SNAPSHOT_TIER_PLATFORM
        };
        let paired_setup = if platform_tier {
            String::new()
        } else {
            format!(
                "mkdir -p /run/nvx/scratch /sys/fs/cgroup; \
             mountpoint -q /sys/fs/cgroup || mount -t cgroup2 none /sys/fs/cgroup; \
             mkdir -p /sys/fs/cgroup/container; \
             mkfs.ext4 -F /dev/vdb >/dev/null; mount -t ext4 /dev/vdb /run/nvx/scratch; \
             printf 'captured-workload-id\\n' >/run/nvx/workload-machine-id; \
             barrier=/run/nvx/test-container-start; mkfifo \"$barrier\"; \
             (IFS= read -r start <\"$barrier\"; [ \"$start\" = start ]; \
             exec unshare --mount --uts --fork --kill-child sh -c 'mount --make-rprivate /; mkdir -p /etc; : >/etc/machine-id; mount --bind /run/nvx/workload-machine-id /etc/machine-id; hostname captured-workload; while [ ! -e /run/nvx/restore-active ]; do sleep 0.01; done; echo {workload_marker}; while :; do sleep 60; done') & workload_pid=$!; \
             echo \"$workload_pid\" >/sys/fs/cgroup/container/cgroup.procs; \
             printf 'start\\n' >\"$barrier\"; rm -f \"$barrier\"; \
             echo \"$workload_pid\" >/run/nvx/container.pid; \
             tries=0; while [ \"$(nsenter -t \"$workload_pid\" -u hostname 2>/dev/null || true)\" != captured-workload ] && [ $tries -lt 200 ]; do sleep 0.01; tries=$((tries+1)); done; \
             [ $tries -lt 200 ] || {{ nvx-exit 67; exit; }};"
            )
        };
        let pre_capture_action = if platform_tier {
            "date -u -s 200001010000.00 >/dev/null; printf 'captured-machine-id\\n' > /etc/machine-id;"
                .to_owned()
        } else {
            "hostname captured-host; printf 'captured-machine-id\\n' > /etc/machine-id;".to_owned()
        };
        let capture_action = if tier == openvmm_helpers::snapshot::SNAPSHOT_TIER_INSTANCE_CHECKPOINT
        {
            "/sbin/nvx-snapshot".to_owned()
        } else {
            format!("/sbin/nvx-snapshot --tier {tier}")
        };
        let repair_action = if tier == openvmm_helpers::snapshot::SNAPSHOT_TIER_INSTANCE_CHECKPOINT
        {
            format!(
                "[ \"$(cat /etc/machine-id)\" = captured-machine-id ]; \
                 [ \"$(cat /run/nvx/workload-machine-id)\" = captured-workload-id ]; \
                 [ \"$(nsenter -t \"$(cat /run/nvx/container.pid)\" -m -r cat /etc/machine-id)\" = captured-workload-id ]; \
                 [ \"$(nsenter -t \"$(cat /run/nvx/container.pid)\" -u hostname)\" = captured-workload ]; \
                 echo {repair_marker};"
            )
        } else {
            String::new()
        };
        let runtime_hook = if platform_tier {
            format!(
                "printf '%s\\n' '#!/bin/sh' 'echo {repair_marker}' 'sleep 1' \
                 '[ \"$(date -u +%Y)\" -ge 2025 ] || exit 1' \
                 'grep -Eq \"^[0-9a-f]{{32}}$\" /etc/machine-id || exit 1' \
                 '[ \"$(hostname)\" = nvx-sandbox ] || exit 1' \
                 'if ! kill -0 \"$(cat /run/nvx/gate-reader.pid)\" 2>/dev/null; then echo {premature_marker}; exit 1; fi' \
                 > /run/nvx/runtime-post-restore; chmod +x /run/nvx/runtime-post-restore;"
            )
        } else if tier == openvmm_helpers::snapshot::SNAPSHOT_TIER_WORKLOAD_START {
            format!(
                "printf '%s\\n' '#!/bin/sh' ': >/run/nvx/restore-active' 'echo {repair_marker}' 'sleep 1' \
                 'grep -Eq \"^[0-9a-f]{{32}}$\" /etc/machine-id || exit 1' \
                 '[ \"$(cat /run/nvx/workload-machine-id)\" = \"$(cat /etc/machine-id)\" ] || exit 1' \
                 '[ \"$(nsenter -t \"$(cat /run/nvx/container.pid)\" -m -r cat /etc/machine-id)\" = \"$(cat /etc/machine-id)\" ] || exit 1' \
                 '[ \"$(nsenter -t \"$(cat /run/nvx/container.pid)\" -u hostname)\" = restored-workload-start ] || exit 1' \
                 'if ! kill -0 \"$(cat /run/nvx/gate-reader.pid)\" 2>/dev/null; then echo {premature_marker}; exit 1; fi' \
                 > /run/nvx/runtime-post-restore; chmod +x /run/nvx/runtime-post-restore;"
            )
        } else {
            String::new()
        };

        let mut capture_args = base_args();
        capture_args.extend([
            "--memory".into(),
            "128M".into(),
            "--kernel".into(),
            kernel.as_os_str().to_owned(),
            "--initrd".into(),
            initrd.as_os_str().to_owned(),
            "--snapshot-destination".into(),
            snapshot_dir.as_os_str().to_owned(),
            "--snapshot-tier".into(),
            tier.into(),
            "--net".into(),
            "10.0.0.2/24".into(),
            "--network-profile".into(),
            "portable".into(),
            "--allow-host".into(),
            "10.0.0.1".into(),
            "--virtio-console".into(),
            format!("listen=tcp:{address}").into(),
            "--microvm-sandbox-block".into(),
            block_arg("distro", &source_layer, true),
            "--microvm-sandbox-block".into(),
            block_arg("scratch", &source_scratch, false),
        ]);
        if !platform_tier {
            capture_args.extend([
                "--cmdline".into(),
                format!("nvx_hostname=restored-{tier}").into(),
            ]);
        }

        let mut source = OpenvmmTestProcess::launch(openvmm.get(), &capture_args)?;
        let mut source_console = TcpConsole::connect(address)
            .with_context(|| format!("capture process {}", source.failure_context()))?;
        source_console.wait_for(MICROVM_BOOT_MARKER)?;
        source_console.wait_for(MICROVM_SHELL_PROMPT)?;
        source_console.send_line(&format!(
            "set -eu; \
             if /sbin/nvx-snapshot --tier {mismatched_tier} >/run/nvx-tier-mismatch.log 2>&1; then nvx-exit 69; exit; fi; \
             grep -q 'requested tier does not match host snapshot tier' /run/nvx-tier-mismatch.log || {{ cat /run/nvx-tier-mismatch.log; nvx-exit 68; exit; }}; \
             echo {mismatch_marker}"
        ))?;
        source_console.wait_for_output_line(mismatch_marker.as_bytes())?;
        source_console.send_line(&format!("{paired_setup} echo {setup_marker}"))?;
        source_console.wait_for_output_line(setup_marker.as_bytes())?;
        source_console.send_line(&format!("{runtime_hook} echo {hook_marker}"))?;
        source_console.wait_for_output_line(hook_marker.as_bytes())?;
        source_console.send_line(&format!(
            "{pre_capture_action} \
             stty -F /dev/hvc1 raw -echo; \
             (dd if=/dev/hvc1 bs=1 count=1 >/dev/null 2>&1; echo {input_marker}) & gate_reader=$!; \
             mkdir -p /run/nvx; echo \"$gate_reader\" > /run/nvx/gate-reader.pid; \
             sleep 0.1; echo {capture_marker}; \
             {capture_action}; \
             {repair_action} \
             wait \"$gate_reader\"; echo {released_marker}; \
             blockdev --flushbufs /dev/vda; \
             value=$(dd if=/dev/vda bs=1 count=1 2>/dev/null | od -An -tu1 | tr -d '[:space:]'); \
             echo {layer_marker}$value; nvx-exit 37"
        ))?;
        source_console.wait_for_output_line(capture_marker.as_bytes())?;
        let (status, output) = source.wait()?;
        anyhow::ensure!(
            status.success(),
            "{tier} capture failed with {status}: {}",
            output_tail(&output)
        );
        let source_output = source_console.finish();
        anyhow::ensure!(
            count_output_lines(&source_output, repair_marker.as_bytes()) == 0
                && count_output_lines(&source_output, mismatch_marker.as_bytes()) == 1,
            "{tier} source crossed its terminal capture boundary: {}",
            output_tail(&source_output)
        );

        let (manifest, _) = openvmm_helpers::snapshot::read_snapshot(&snapshot_dir, MEMORY_BYTES)?;
        anyhow::ensure!(
            manifest.snapshot_tier == tier
                && manifest.restore_policy == restore_policy
                && manifest.consumed_config_sections == consumed_sections,
            "{tier} manifest policy is inconsistent"
        );
        let layer = &manifest
            .machine_contract
            .as_ref()
            .context("tier snapshot is missing its machine contract")?
            .microvm_sandbox_blocks[0];
        anyhow::ensure!(
            (tier == openvmm_helpers::snapshot::SNAPSHOT_TIER_PLATFORM)
                == (layer.identity_kind == "unbound" && layer.identity.is_empty()),
            "{tier} layer binding does not match its sharing scope"
        );

        let restore_layer = if tier == openvmm_helpers::snapshot::SNAPSHOT_TIER_PLATFORM {
            &replacement_layer
        } else {
            &source_layer
        };
        let mut restore_args = base_args();
        restore_args.extend([
            "--restore-snapshot".into(),
            snapshot_dir.as_os_str().to_owned(),
            "--restore-entropy".into(),
            "--network-profile".into(),
            "portable".into(),
            "--allow-host".into(),
            "10.0.0.1".into(),
            "--microvm-sandbox-block".into(),
            block_arg("distro", restore_layer, true),
        ]);
        if tier == openvmm_helpers::snapshot::SNAPSHOT_TIER_PLATFORM {
            restore_args.extend([
                "--microvm-sandbox-block".into(),
                block_arg("scratch", &restore_scratch, false),
            ]);
        }

        if single_use {
            let invalid_profile_args = [
                "--single-process".into(),
                "--hypervisor".into(),
                hypervisor.into(),
                "--restore-snapshot".into(),
                snapshot_dir.as_os_str().to_owned(),
            ];
            let invalid = OpenvmmTestProcess::launch(openvmm.get(), &invalid_profile_args)?;
            let (status, output) = invalid.wait()?;
            anyhow::ensure!(
                !status.success()
                    && contains_bytes(
                        &output,
                        b"microVM snapshot restore requires --machine microvm, microvm-v2, or microvm-v3"
                    )
                    && !snapshot_dir.join("resume.claim").exists(),
                "wrong-profile restore consumed or entered an instance checkpoint: {}",
                output_tail(&output)
            );
        }

        let mut restore = OpenvmmTestProcess::launch(openvmm.get(), &restore_args)?;
        let mut restore_console = TcpConsole::connect(address)
            .with_context(|| format!("restore process {}", restore.failure_context()))?;
        restore_console.send_bytes(b"Z")?;
        restore_console
            .wait_for_output_line(repair_marker.as_bytes())
            .with_context(|| format!("restore process {}", restore.failure_context()))?;
        restore_console
            .wait_for_output_line(released_marker.as_bytes())
            .with_context(|| format!("restore process {}", restore.failure_context()))?;
        if tier == openvmm_helpers::snapshot::SNAPSHOT_TIER_WORKLOAD_START {
            restore_console
                .wait_for_output_line(workload_marker.as_bytes())
                .with_context(|| format!("restore process {}", restore.failure_context()))?;
        }
        let (status, output) = restore.wait()?;
        anyhow::ensure!(
            status.code() == Some(37),
            "{tier} restore failed with {status}: {}",
            output_tail(&output)
        );
        let restore_output = restore_console.finish();
        anyhow::ensure!(
            count_output_lines(&restore_output, input_marker.as_bytes()) == 1
                && count_output_lines(&restore_output, premature_marker.as_bytes()) == 0,
            "{tier} restore input crossed the gate before acknowledgement: {}",
            output_tail(&restore_output)
        );
        let expected_layer = if tier == openvmm_helpers::snapshot::SNAPSHOT_TIER_PLATFORM {
            195
        } else {
            60
        };
        anyhow::ensure!(
            count_output_lines(
                &restore_output,
                format!("{layer_marker}{expected_layer}").as_bytes()
            ) == 1,
            "{tier} restore observed the wrong layer binding: {}",
            output_tail(&restore_output)
        );

        if tier == openvmm_helpers::snapshot::SNAPSHOT_TIER_WORKLOAD_START {
            let mut timeout_args = restore_args.clone();
            timeout_args.extend(["--restore-gate-timeout-ms".into(), "500".into()]);
            let mut timeout_restore = OpenvmmTestProcess::launch(openvmm.get(), &timeout_args)?;
            let mut timeout_console = TcpConsole::connect(address).with_context(|| {
                format!(
                    "timeout restore process {}",
                    timeout_restore.failure_context()
                )
            })?;
            timeout_console.send_bytes(b"Z")?;
            let (status, output) = timeout_restore.wait()?;
            let timeout_output = timeout_console.finish();
            anyhow::ensure!(
                !status.success()
                    && count_output_lines(&timeout_output, input_marker.as_bytes()) == 0
                    && count_output_lines(&timeout_output, released_marker.as_bytes()) == 0
                    && count_output_lines(&timeout_output, workload_marker.as_bytes()) == 0,
                "workload-start restore gate timeout released input, thawed the workload, or succeeded; status={status}; process output: {}; console output: {}",
                output_tail(&output),
                output_tail(&timeout_output)
            );
        }

        if single_use {
            let duplicate = OpenvmmTestProcess::launch(openvmm.get(), &restore_args)?;
            let (status, output) = duplicate.wait()?;
            anyhow::ensure!(
                !status.success()
                    && contains_bytes(&output, b"resume snapshot has already been claimed"),
                "duplicate instance-checkpoint restore was not rejected: {}",
                output_tail(&output)
            );
        }
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

#[openvmm_test_no_agent(microvm_pvh_x64)]
async fn microvm_v2_sandbox_blocks(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    use disk_backend_resources::LayeredDiskHandle;
    use disk_backend_resources::layer::RamDiskLayerHandle;
    use openvmm_defs::config::LoadMode;
    use openvmm_defs::config::MICROVM_ABI_VERSION_2;
    use openvmm_defs::config::MachineProfile;
    use openvmm_defs::config::MicrovmSandboxBlockConfig;
    use openvmm_defs::config::MicrovmSandboxBlockRole;
    use openvmm_defs::config::VirtioBus;
    use openvmm_defs::config::append_microvm_v2_virtio_discovery;
    use virtio_resources::blk::VirtioBlkHandle;

    const TIMEOUT: Duration = Duration::from_secs(30);
    const DISK_SIZE: u64 = 8 * 1024 * 1024;
    const WORKLOAD: &[u8] = br#"#!/bin/sh
set -eu
for name in vda vdb vdc vdd; do
    tries=0
    while [ ! -b "/dev/$name" ] && [ "$tries" -lt 200 ]; do
        sleep 0.05
        tries=$((tries + 1))
    done
    [ -b "/dev/$name" ] || exit 20
done
grep -q 'virtio_mmio.device=0x1000@0xd0003000:4' /proc/cmdline || exit 21
grep -q 'virtio_mmio.device=0x1000@0xd0004000:12' /proc/cmdline || exit 22
grep -q 'virtio_mmio.device=0x1000@0xd0005000:9' /proc/cmdline || exit 23
grep -q 'virtio_mmio.device=0x1000@0xd0006000:11' /proc/cmdline || exit 24
[ "$(cat /sys/block/vda/ro)" = 1 ] || exit 25
[ "$(cat /sys/block/vdb/ro)" = 1 ] || exit 26
[ "$(cat /sys/block/vdc/ro)" = 1 ] || exit 27
[ "$(cat /sys/block/vdd/ro)" = 0 ] || exit 28
printf MICROVM-V2-SCRATCH-OK | dd of=/dev/vdd bs=512 count=1 conv=sync,notrunc 2>/dev/null
[ "$(dd if=/dev/vdd bs=512 count=1 2>/dev/null | head -c 22)" = MICROVM-V2-SCRATCH-OK ] || exit 29
/sbin/nvx-exit 37
while :; do sleep 3600; done
"#;

    let modified_initrd =
        config.prepare_initrd_with_file("microvm-v2-sandbox-blocks.sh", WORKLOAD, 0o100755)?;
    let roles = [
        MicrovmSandboxBlockConfig {
            role: MicrovmSandboxBlockRole::Distro,
            read_only: true,
        },
        MicrovmSandboxBlockConfig {
            role: MicrovmSandboxBlockRole::Runtime,
            read_only: true,
        },
        MicrovmSandboxBlockConfig {
            role: MicrovmSandboxBlockRole::Custom,
            read_only: true,
        },
        MicrovmSandboxBlockConfig {
            role: MicrovmSandboxBlockRole::Scratch,
            read_only: false,
        },
    ];
    let disks = roles.map(|_| {
        LayeredDiskHandle::single_layer(RamDiskLayerHandle {
            len: Some(DISK_SIZE),
            sector_size: None,
        })
        .into_resource()
    });

    let mut vm = config
        .with_prebuilt_initrd(modified_initrd.to_path_buf())
        .with_microvm_machine()
        .modify_backend(move |backend| {
            backend.with_custom_config(|config| {
                config.machine_profile = MachineProfile::Microvm {
                    abi_version: MICROVM_ABI_VERSION_2,
                };
                let LoadMode::Pvh { cmdline, .. } = &mut config.load_mode else {
                    panic!("microVM test did not produce PVH load mode");
                };
                cmdline.push_str(" nvx_exec=/microvm-v2-sandbox-blocks.sh");
                append_microvm_v2_virtio_discovery(cmdline, None, None, false, &roles).unwrap();
                config.microvm_sandbox_blocks = roles.to_vec();
                config
                    .virtio_devices
                    .extend(roles.into_iter().zip(disks).map(|(block, disk)| {
                        (
                            VirtioBus::Mmio,
                            VirtioBlkHandle {
                                disk,
                                read_only: block.read_only,
                            }
                            .into_resource(),
                        )
                    }));
            })
        })
        .run_without_agent()
        .await?;

    let halt = CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.wait_for_halt())
        .await
        .context("timed out waiting for microVM v2 sandbox-block workload")??;
    assert_eq!(halt.reason, PetriHaltReason::PowerOff);
    assert!(
        halt.detail.contains("code: 37"),
        "microVM v2 sandbox-block workload failed: {}",
        halt.detail
    );
    vm.teardown().await
}

#[vmm_test_with(openvmm, noagent, configs(microvm_pvh_x64))]
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
    fail() { nvx-exit "$1"; exit 1; }
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
[ -n "$device" ] || fail 20
grep -q 'virtio_mmio.device=0x1000@0xd0000000:' /proc/cmdline || fail 21
grep -q 'virtnet_ip=10.0.0.2' /proc/cmdline || fail 22
grep -q 'virtnet_mask=255.255.255.0' /proc/cmdline || fail 23
grep -q 'virtnet_gw=10.0.0.1' /proc/cmdline || fail 24
[ "$(cat /sys/class/net/$device/address)" = '52:54:00:00:00:02' ] || fail 25
grep -qi 'd0000000-d0000fff.*virtio' /proc/iomem || fail 26
ifconfig "$device" 10.0.0.2 netmask 255.255.255.0 up || fail 27
route add default gw 10.0.0.1 dev "$device" 2>/dev/null || true
ping -c 1 -W 5 10.0.0.1 >/dev/null || fail 28
nvx-exit 37
"#;

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
        .with_microvm_machine()
        .modify_backend(move |backend| {
            backend.with_custom_config(|config| {
                let LoadMode::Pvh { cmdline, .. } = &mut config.load_mode else {
                    panic!("microVM test did not produce PVH load mode");
                };
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

    vm.backend().write_microvm_portb_input(WORKLOAD).await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(
            vm.backend()
                .wait_for_microvm_portb_output("ALPINE-MICROVM-BOOT-OK"),
        )
        .await
        .context("timed out waiting for microVM boot marker")??;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_output("/ # "))
        .await
        .context("timed out waiting for microVM shell prompt")??;

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
