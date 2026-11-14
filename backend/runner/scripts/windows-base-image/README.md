# Windows job base image

This is Codeyard's QEMU backend, not an upstream actions/runner feature. Windows
Server 2025 Evaluation (English x64, Desktop Experience) is the supported image.
No Windows media or licence is included or downloaded by these scripts. Evaluation
images expire; the administrator must comply with Microsoft's terms and rebuild
or supply a properly licensed image as needed. macOS is not supported.

## Build

1. Download the **official Windows Server 2025 evaluation ISO** from Microsoft's
   Evaluation Center yourself, verify it, and review/accept its licence.
2. On a Linux host install `qemu-system-x86_64`, `qemu-img`, `xorriso`, Python 3,
   GNU coreutils, util-linux (`setsid`), and OpenSSH 9+ clients (`ssh`, `scp`, `ssh-keygen`),
   or enter `nix develop .#vm`. The runtime needs those tools except Python. KVM is
   the default; allow the service user access to `/dev/kvm` when available.
3. Run (the output must not already exist):

   ```sh
   bash backend/runner/scripts/windows-base-image/build.sh \
     --accept-license /srv/iso/windows-server-2025-eval.iso /srv/images/windows.qcow2
   ```

If KVM is unavailable, explicitly set `WINDOWS_ALLOW_TCG=1` on the build command
above to permit software emulation. Usable KVM is still preferred. TCG installation
can take much longer; set `WINDOWS_INSTALL_TIMEOUT=6h` if needed.

The default ISO image index is **2: Standard Evaluation (Desktop Experience)**.
Inspect your ISO's image list with DISM and set `WINDOWS_IMAGE_INDEX` if different.
The answer file wipes **only the new guest disk**, uses BIOS/MBR, and runs setup
unattended. Set `WINDOWS_DISK_SIZE` (default `80G`) or `WINDOWS_INSTALL_TIMEOUT`
(default `2h`) if necessary. Memory is 8 GiB and CPU count is 4 during installation.
Watch VNC on **127.0.0.1:5901** (tunnel it, never expose it); if Windows asks to boot
from CD, press a key there. The script requires a normal QEMU exit and verifies a
serial success marker before retaining the candidate image, and removes partial
images and staging on failure/cancellation.
A failed provisioning step leaves the console open until the host timeout; see
`C:\ProgramData\Codeyard\provision.log`.

Provisioning needs outbound HTTPS and Windows Update for OpenSSH's optional
feature. It installs signed PowerShell 7.4, Git for Windows, and the current Node
LTS from their official distribution sites, checking installer Authenticode
signatures. Review/update pinned PowerShell and Git releases in `Provision.ps1`
before building. Windows Update policies, ISO edition/language differences, and
network restrictions can require administrator adjustments. No Sysprep is run:
each disposable overlay starts with the same machine identity but new SSH keys.

The generated answer file contains a random, temporary Administrator password.
It is held in a mode-0700 temporary directory, then deleted. Provisioning disables
auto-login and Administrator, deletes cached answer files, creates an administrator
`runner` account with an unknown random password, and shuts down. Only public-key
SSH login is enabled. Protect the host and image directory; the base is chmod 0400,
and should be writable only by an administrator during deliberate maintenance.

## Runtime and seed contract

This is an experimental library API, not a connected workflow executor. `vm:`
platform mappings are explicitly rejected rather than sent to Podman as image names.
Call `vm::JobVm::start`, `copy_in`, `run`, `copy_out`, then `stop` directly. The default workspace is `C:/codeyard/workspace`. These APIs expect absolute
Windows guest paths and execute PowerShell scripts using GitHub's Stop/LASTEXITCODE
conventions. Output streams line by line. Directory copies use SFTP-backed `scp -r`,
include dotfiles, merge into the destination, and do **not** propagate deletions.
Uploads require a Linux host and transfer a private descriptor-opened snapshot;
symbolic links and special files are rejected without following them. Snapshots are
limited to 100,000 entries, 128 directory levels, and 16 GiB. SSH exit status is
limited to 0–255; 255 also denotes transport failure,
whose diagnostics are sent to stderr.

Every job gets a fresh qcow2 overlay and Ed25519 client key. Its public key is put
in a read-only ISO labelled **CODEYARD**, with one root file `authorized_keys`.
`Import-JobKey.ps1`, installed as a SYSTEM startup task, removes any old authorized
key, waits for this CD, installs only that key with SYSTEM/Administrators ACLs,
generates new SSH host keys, and starts sshd. The guest SSH firewall rule accepts
only QEMU's default host gateway (`10.0.2.2`), on any Windows network profile;
Windows may classify this disposable network as Public. Missing/invalid seed means no SSH.
The task and sshd configuration are part of the image contract; custom images must
provide the same account, directories, PowerShell, and seed handling. Leave the
OpenSSH command shell as `cmd.exe /c`: the runtime explicitly invokes `pwsh`, and
using PowerShell as the outer SSH shell can collapse nonzero native exit codes.

QEMU uses q35 with SATA disks and an e1000e NIC (no extra virtio drivers needed),
user-mode networking, and only a loopback ephemeral SSH forward. Host keys are
TOFU-pinned in a private per-job known-hosts file. KVM is required by default; the
caller may explicitly permit TCG, which prints a warning and is very slow.
Outbound guest networking is enabled: this is not a network sandbox. Do not run
untrusted jobs where they can reach sensitive host/LAN services. Guests must never
be given host credentials or host mounts.

Normal stop requests ACPI shutdown, then kills QEMU after the grace period. Dropping
`JobVm`, including cancellation of the task that owns it, kills QEMU and unlinks its
job files. Cancelling one borrowed method does not drop a VM retained by its caller. Like
all process-local cleanup, this cannot handle host power failure or SIGKILL of the
runner; remove stale `codeyard-vm-*` directories after verifying no QEMU uses them.

## Validate before use

The builder has run real Windows Server 2025 Evaluation guest checks under TCG
on 2026-11-12. The first attempt on the manually provisioned evaluation image
booted and imported its key, but timed out reaching SSH: the bundled firewall
rule covered only Private networks. A maintained copy with the corrected seed
script then passed the real guest test in 964 seconds under TCG: Windows
10.0.26100.0, PowerShell 7.4.13, Git 2.49.0.windows.1, Node 26.13.1, transfers,
environment, nonzero exit status and cleanup. An independent boot of that
maintained copy with no diagnostic connections passed the same assertions in
1,024 seconds. Those two passes validate the maintained image, not a fresh
unattended build.

A separate, fully unattended installation then completed normally without manual
input at 2026-11-12T08:22:43Z and published a read-only candidate. The build script
requires a normal QEMU exit and the `CODEYARD_IMAGE_READY` serial marker before
publication; its staging cleanup removes the raw marker log, so readiness rests
on those enforced checks rather than a saved marker. The candidate is
`windows-unattended.qcow2`, 14,539,030,528 bytes, mode 0400, SHA-256
`f56b4f7235d89cbe22422b67452d04e593cf63a477892c2110a95172a2fa2a2e`. The same
ignored test passed it: 1 passed, 0 failed, 0 ignored in 1,034.19 seconds, with
tools, transfers, environment, nonzero exit status, download and cleanup
verified. The candidate SHA-256 was unchanged afterwards.

All of these runs used the experimental library API and TCG software emulation,
not KVM, and none establishes Windows workflow support. The passing runs verify the
library and the tested image digests, not an arbitrary custom image. With a
provisioned image:

```sh
CODEYARD_WINDOWS_IMAGE=/srv/images/windows.qcow2 \
  cargo test -p codeyard-runner vm::tests::windows_boot_step_and_copy -- --ignored --nocapture
```

Record that image's SHA-256 with the result. The unattended candidate above is
validated only at the digest shown.

For an explicit software-emulation fallback, add `CODEYARD_VM_ALLOW_TCG=1` and
`CODEYARD_VM_BOOT_TIMEOUT_SECONDS=1800` to that command. The boot timeout defaults
to five minutes and accepts 1–7200 seconds; extending it does not change step assertions.

This ignored test checks a real Windows boot, PowerShell/Git/Node availability,
hidden-file upload, environment and streamed output, nonzero step exit, download,
shutdown and overlay removal. Regular unit tests do not
need QEMU. Run it after every image/script change before using the experimental VM API.

`pwsh -NoProfile -File tests/vm/test-windows-firewall.ps1` checks existing/missing
firewall rules and failure propagation using mocked cmdlets, including on Linux.
It does not replace the real Windows guest test.
