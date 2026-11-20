# ublk520

Expose a disk with 520-byte sectors as a 512-byte-sector block device (`/dev/ublkbN`)
using Linux ublk (kernel 6.0+). Virtual sector N = first 512 bytes of physical block N;
the trailing 8 bytes are ignored on read and written as zeros on write.
Capacity is the full block count x 512 (about 1.5% less than the drive's raw size).

Only for drives with protection information (PI) disabled; the program refuses to run otherwise.

## Build
Needs Rust >= 1.80 (use rustup) and libclang (bindgen): `sudo apt install libclang-dev build-essential`

    cargo build --release
    cargo test --release

## Run
    sudo modprobe ublk_drv

    # 1. safe test with a fake 520-byte disk (a file):
    truncate -s 1G fake520.img
    sudo ./target/release/ublk520 --file fake520.img     # prints "ready: /dev/ublkb0"
    # in another shell: mkfs.ext4 /dev/ublkb0; mount; copy files; umount; Ctrl-C the program

    # 2. the real drive (find its sg node with `lsscsi -g`; it can change between boots):
    sudo ./target/release/ublk520 /dev/sg4

Ctrl-C / SIGTERM flushes and detaches. Unmount before stopping it.

## systemd (optional)
    [Unit]
    Description=ublk520 shim for 520-byte SSD
    After=systemd-modules-load.service

    [Service]
    ExecStartPre=/sbin/modprobe ublk_drv
    ExecStart=/usr/local/bin/ublk520 /dev/sg4
    Restart=on-failure

    [Install]
    WantedBy=multi-user.target

Mount by filesystem UUID with `nofail`, since /dev/ublkbN only exists after the service starts.
