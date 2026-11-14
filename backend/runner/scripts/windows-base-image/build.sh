#!/usr/bin/env bash
# QEMU image provisioning is Codeyard-specific, not ported from actions/runner.
set -euo pipefail

if [[ $# != 3 || $1 != --accept-license ]]; then
    echo "Usage: $0 --accept-license /path/to/official-evaluation.iso /path/to/base.qcow2" >&2
    echo "Download Windows Server 2025 Evaluation yourself and review its licence first." >&2
    exit 2
fi
for tool in qemu-img qemu-system-x86_64 xorriso python3 setsid timeout; do
    command -v "$tool" >/dev/null || { echo "Missing $tool" >&2; exit 1; }
done
accel=kvm
cpu=host
if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
    [[ ${WINDOWS_ALLOW_TCG:-0} == 1 ]] || {
        echo 'KVM is unavailable; set WINDOWS_ALLOW_TCG=1 to permit slow software emulation.' >&2
        exit 1
    }
    accel=tcg
    cpu=max
    echo 'KVM is unavailable: using TCG software emulation for image installation.' >&2
fi
iso=$(realpath "$2")
output=$(realpath -m "$3")
[[ -f "$iso" && ! -e "$output" ]] || { echo 'ISO must exist; output must not already exist.' >&2; exit 1; }
source_dir=$(cd -- "$(dirname -- "$0")" && pwd)
umask 077
stage=$(mktemp -d "$(dirname -- "$output")/.codeyard-windows-build.XXXXXXXX")
image="$stage/image.qcow2"
installer=
cleanup() {
    if [[ -n $installer ]]; then
        kill -TERM -- "-$installer" 2>/dev/null || true
        for ((attempt=0; attempt<100; attempt++)); do
            kill -0 -- "-$installer" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL -- "-$installer" 2>/dev/null || true
        wait "$installer" 2>/dev/null || true
    fi
    rm -rf -- "$stage"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$stage/files"
cp "$source_dir/Provision.ps1" "$source_dir/Import-JobKey.ps1" "$stage/files/"
python3 - "$source_dir/autounattend.xml" "$stage/files/autounattend.xml" "${WINDOWS_IMAGE_INDEX:-2}" <<'PY'
import pathlib, secrets, sys
source, destination, index = sys.argv[1:]
if not index.isdecimal() or int(index) < 1:
    raise SystemExit('WINDOWS_IMAGE_INDEX must be positive')
password = secrets.token_urlsafe(36) + 'aA1!'
xml = pathlib.Path(source).read_text().replace('@ADMIN_PASSWORD@', password).replace('@IMAGE_INDEX@', index)
pathlib.Path(destination).write_text(xml)
PY
# Keep the generated answer file and its temporary password private.
xorriso -as mkisofs -quiet -J -r -V CYINSTALL -o "$stage/unattended.iso" "$stage/files"
qemu-img create -f qcow2 "$image" "${WINDOWS_DISK_SIZE:-80G}"
# Escape commas in QEMU's comma-separated drive options.
escape_path() { printf '%s' "${1//,/,,}"; }
echo 'Installing Windows. Follow the console at 127.0.0.1:5901 (VNC); installation shuts down when complete.'
echo 'A failed provisioning step leaves Windows running; inspect C:\ProgramData\Codeyard\provision.log in the console.'
# timeout terminates a hung installation and the EXIT trap deletes the partial image.
setsid timeout --foreground --kill-after=10s "${WINDOWS_INSTALL_TIMEOUT:-2h}" qemu-system-x86_64 \
    -name codeyard-windows-install -machine "q35,accel=$accel" -cpu "$cpu" -smp 4 -m 8192 \
    -drive "file=$(escape_path "$image"),format=qcow2,if=ide,index=0" \
    -drive "file=$(escape_path "$iso"),media=cdrom,readonly=on,if=ide,index=2" \
    -drive "file=$(escape_path "$stage/unattended.iso"),media=cdrom,readonly=on,if=ide,index=3" \
    -boot order=c,once=d -netdev user,id=net0 -device e1000e,netdev=net0 \
    -vnc 127.0.0.1:1 -monitor none -serial "file:$stage/serial.log" &
installer=$!
status=0
wait "$installer" || status=$?
installer=
(( status == 0 )) || exit "$status"
grep -q '^CODEYARD_IMAGE_READY' "$stage/serial.log" || { echo 'Provisioning did not complete.' >&2; exit 1; }
chmod 0400 "$image"
# Publishing a new directory entry is atomic and never replaces another build's image.
ln -T -- "$image" "$output"
echo "Candidate base image: $output"
echo 'Validate with the ignored windows_boot_step_and_copy test; workflow integration is not supported.'
