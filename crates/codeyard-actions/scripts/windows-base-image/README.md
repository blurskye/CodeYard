# Windows job base image

This is Codeyard's QEMU backend, not an upstream actions/runner feature. Windows
Server 2025 Evaluation (English x64, Desktop Experience) is the supported image.
No Windows media or licence is included or downloaded by these scripts. Evaluation
images expire; the administrator must comply with Microsoft's terms and rebuild
or supply a properly licensed image as needed. macOS is not supported.

## Build

1. Download the **official Windows Server 2025 evaluation ISO** from Microsoft's
   Evaluation Center yourself, verify it, and review/accept its licence.
2. On a Linux KVM host install `qemu-system-x86_64`, `qemu-img`, `xorriso`, Python 3,
   GNU coreutils, util-linux (`setsid`), and OpenSSH 9+ clients (`ssh`, `scp`, `ssh-keygen`). The runtime
   needs those tools except Python. Allow the service user access to `/dev/kvm`.
3. Run (the output must not already exist):

   ```sh
   bash crates/codeyard-actions/scripts/windows-base-image/build.sh \
     --accept-license /srv/iso/windows-server-2025-eval.iso /srv/images/windows.qcow2
   ```

The default ISO image index is **2: Standard Evaluation (Desktop Experience)**.
Inspect your ISO's image list with DISM and set `WINDOWS_IMAGE_INDEX` if different.
The answer file wipes **only the new guest disk**, uses BIOS/MBR, and runs setup
unattended. Set `WINDOWS_DISK_SIZE` (default `80G`) or `WINDOWS_INSTALL_TIMEOUT`
(default `2h`) if necessary. Memory is 8 GiB and CPU count is 4 during installation.
Watch VNC on **127.0.0.1:5901** (tunnel it, never expose it); if Windows asks to boot
from CD, press a key there. The script verifies a serial success marker before
retaining the candidate image, and removes partial images on failure/cancellation.
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
generates new SSH host keys, and starts sshd. Missing/invalid seed means no SSH.
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

Normal stop requests ACPI shutdown, then kills QEMU after the grace period. Drop
and async task cancellation kill QEMU immediately and unlink all job files. Like
all process-local cleanup, this cannot handle host power failure or SIGKILL of the
runner; remove stale `codeyard-vm-*` directories after verifying no QEMU uses them.

## Validate before use

Real Windows boots have **not** been exercised on the builder. On the KVM machine:

```sh
CODEYARD_WINDOWS_IMAGE=/srv/images/windows.qcow2 \
  cargo test -p codeyard-actions vm::tests::windows_boot_step_and_copy -- --ignored --nocapture
```

This ignored test checks boot, hidden-file upload, environment and streamed output,
nonzero step exit, download, shutdown and overlay removal. It is marked
`#[ignore = "needs kvm and a windows image"]`; regular unit tests do not need QEMU.
Run it after every image/script change before using the experimental VM API.
