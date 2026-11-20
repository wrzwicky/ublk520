# ublk520

Expose a disk with 520-byte sectors as a normal 512-byte-sector block device (`/dev/ublkbN`)
using Linux ublk (kernel 6.0+). Linux's `sd` driver refuses non-power-of-two sector sizes
(`Unsupported sector size 520`), so such drives (NetApp/EMC-formatted enterprise disks) show
up with 0 bytes. This program talks to the drive directly with SG_IO and translates.

- Virtual 512-byte sector N = first 512 bytes of physical 520-byte block N.
- The trailing 8 bytes of each block are ignored on read and written as zeros on write.
- Capacity = (physical blocks) x 512, about 1.5% less than the drive's raw size.
- Refuses to start if the drive has protection information (PI/DIF) enabled.
- Flush is passed through (`SYNCHRONIZE CACHE(16)`); discard/TRIM is not advertised.
- SIGINT/SIGTERM flushes and detaches cleanly. **Unmount (or `zpool export`) first.**

## Build

Needs Rust >= 1.80 (use rustup; distro packages are often older) and libclang (bindgen):

    sudo apt install libclang-dev build-essential
    cargo build --release
    cargo test --release          # unit tests for the 512<->520 translation (file-backed, no hardware)
    sudo install -m755 target/release/ublk520 /usr/local/bin/

## Run

    sudo modprobe ublk_drv

    # Safe first test: a fake 520-byte "disk" in a regular file
    truncate -s 1G fake520.img
    sudo ublk520 --file fake520.img        # prints "ready: /dev/ublkb0"

    # The real drive (find its sg node with `lsscsi -g`; it can change between boots)
    sudo ublk520 /dev/sg4
    sudo ublk520 --id 0 /dev/sg4           # force /dev/ublkb0

Then `mkfs`/mount `/dev/ublkbN` as usual. Ctrl-C (or SIGTERM) to stop.

## Tuned settings (constants at the top of `src/main.rs`)

| Constant | Value | Why |
|---|---|---|
| `MAX_BLOCKS_PER_SCSI_COMMAND` | 1024 | Max 520-byte blocks per SG_IO command. A request never waits for or pads to this; it is only an upper bound. Check `/sys/block/<sdX>/queue/max_hw_sectors_kb` (was 16383 here, so no HBA limit). |
| `UBLK_MAX_REQUEST_BYTES` | 512 KiB | Largest request the kernel will send us (1024 virtual sectors = one SG_IO). |
| `UBLK_QUEUE_COUNT` | 32 | One ublk queue per CPU. Each queue handles one request at a time, so parallelism comes from queues. Fewer queues than CPUs means jobs on CPUs that share a queue serialize. |
| `UBLK_QUEUE_DEPTH` | 8 | The handler is synchronous, so more depth only costs memory (32 x 8 x 512 KiB = 128 MiB total). |

Known limit: a single-threaded workload (one `dd`, one `cp`, one DB connection) lands on a
single queue and is serial. Large requests help (one command per request); small ones do not.
A fully async handler (many SG_IO in flight per queue) would fix this but was not needed.

## Auto-start with systemd

Files in `contrib/`:
- `ublk520-run.sh`: finds the drive's sg node at boot (sg numbers move) by matching the
  `lsscsi` model string (default `MZ-3S910`; override with `UBLK520_MODEL`) **and** requiring
  a logical block length of exactly 520, then runs `ublk520 --id 0`. Needs `lsscsi` and `sg3-utils`.
- `ublk520.service`: runs the wrapper, and does not report "started" until `/dev/ublkb0`
  exists, so anything ordered after it waits for the device. Uses `DefaultDependencies=no`
  (without it the unit creates an ordering cycle with `local-fs.target`; `systemd-analyze verify`
  caught that). It never SIGKILLs the shim on stop.

Install:

    sudo install -m755 contrib/ublk520-run.sh /usr/local/bin/
    sudo install -m644 contrib/ublk520.service /etc/systemd/system/
    sudo systemctl daemon-reload && sudo systemctl enable --now ublk520

Plain filesystem (ext4/xfs/...): mount by filesystem UUID, since ublk devices have no
`/dev/disk/by-id` entry. `nofail` keeps the boot going if the drive is missing:

    UUID=xxxx-xxxx  /mnt/ssd  ext4  defaults,nofail,x-systemd.requires=ublk520.service  0  2

## ZFS

    sudo zpool create -o ashift=12 tank /dev/ublkb0

- Force `ashift=12`: the shim advertises 512-byte sectors, so ZFS would pick 9 on its own.
  4 KiB alignment is also kinder to the SSD and gives the shim larger requests.
- ZFS has no use for the 8-byte tail: its checksums live in parent block pointers.
- Pin the device id (`--id 0`, as the wrapper does) so the pool always finds `/dev/ublkb0`.
- Install the ZFS drop-in so the shim starts before pools are imported and the pool is
  exported before the shim stops:

      sudo mkdir -p /etc/systemd/system/ublk520.service.d
      sudo install -m644 contrib/ublk520-zfs.conf /etc/systemd/system/ublk520.service.d/zfs.conf
      sudoedit /etc/systemd/system/ublk520.service.d/zfs.conf    # set your pool name
      sudo systemctl daemon-reload

  It orders the shim `Before=` `zfs-import-cache.service`, `zfs-import-scan.service` and
  `zfs-import.target` (check names with `systemctl list-units 'zfs*'`; they differ by distro),
  and adds `ExecStop=-/usr/sbin/zpool export <pool>` with a long stop timeout. If the export
  fails (pool busy), the shim still stops and ZFS will suspend the pool, so stop whatever uses it first.
- Make sure the pool is in the cachefile (`zpool set cachefile=/etc/zfs/zpool.cache tank`) so
  `zfs-import-cache` picks it up at boot.
- Never stop or kill the shim while the pool is imported (`zpool export` first).

## How we got here (development and testing log)

1. **Diagnosis.** `sd` rejected the drive (`Unsupported sector size 520`, 0 B, 520-byte physical).
   The Dell H310 Mini (MegaRAID SAS 2008) flagged it `UGUnsp` (unsupported) and hid it from Linux.
   Moving it to an IT-mode SAS3008 HBA exposed it as a *SATA* drive (`ATA SAMSUNG MZ-3S910`; an early
   guess that it was SAS was wrong, which is why `sg_format` returned "illegal request").
   `hdparm --set-sector-size` failed with `READ_LOG_EXT(SECTOR_CONFIGURATION) failed`, and
   SMART log 0x2F was absent, so the 520-byte format is fixed in firmware.
2. **Shim v1.** Rust + `libublk`, synchronous SG_IO, one queue. Built with Rust 1.85 +
   libclang; unit tests cover multi-chunk round trips, the physical layout (data in the first
   512 bytes, zero 8-byte tail, neighbours untouched), tail clearing on overwrite, and the
   `sg_io_hdr` struct size (88 bytes).
3. **First real run.** Worked on the first try: about 360 MB written in 6 s.
4. **Where the speed goes.** Raw drive ceiling via the HBA (read-only):

   | Test | Result |
   |---|---|
   | `sg_dd`, 1 command in flight, 128 blocks | 87 MB/s |
   | `sg_dd`, 1 in flight, 512 blocks | 136 MB/s |
   | `sgp_dd`, 4 threads, 128 / 512 / 1024 blocks | 215 / 229 / 234 MB/s |

   So queue depth matters much more than command size once commands overlap.
5. **Queue scaling** (fio, 4 jobs, 64 KB random reads): 1 queue 61 MB/s, 4 queues 93, 8 queues 137.
   Sublinear because jobs shared queues (`/sys/block/ublkb0/mq/*/cpu_list` shows the CPU-to-queue
   map). Pinned to CPUs in different queues: 187 MB/s. 32 queues at depth 8, unpinned: 188 MB/s
   (about 87% of `sgp_dd` at the same command size). That became the default.
6. **Integrity.** `fio --verify=crc32c --verify_fatal=1`: a single 64 KB job (4 GB, ~52 MB/s write,
   ~65 MB/s read) and 8 parallel jobs with separate files at 16 KB (~9 MB/s per job each way):
   no verify errors. An early version of the second test was flawed (jobs overwriting each
   other's blocks would cause false failures); use per-job files.
7. **Restart test.** Unmount, stop the shim (flush and detach), start it again, `fsck`, mount,
   data intact (tested with exFAT; `fsck.exfat` reported clean).

## Not tested / caveats

- ZFS, the systemd units and the wrapper script were not run end to end here. The unit passed
  `systemd-analyze verify` (aside from the not-yet-installed binaries) and the script passed
  `bash -n`, but the `sg_readcap` output match (`Logical block length=520`) was not checked
  against a real drive. Run `sg_readcap -l /dev/sgN` and adjust if your sg3-utils differs.
- Hard power loss mid-write: the shim forwards flushes, but whether recent writes survive
  depends on the drive's own cache. Only the orderly stop/start path was tested.
- exFAT (used in testing) has no journal; prefer ext4, XFS or ZFS for data you care about.
- 8-byte tails are zeroed on every write, so don't use this on a drive that has PI enabled
  or whose existing 520-byte contents you need to preserve.

## Troubleshooting

- `dmesg | grep -i -E "ublk|mpt3sas|sg[0-9]"` for kernel-side errors.
- The shim logs every failed request to stderr (`journalctl -u ublk520`), with the SCSI status and sense bytes.
- Drive invisible to `lsscsi`: it is probably behind a RAID-mode controller (PERC/MegaRAID) that
  hides drives it can't use. Put it on an IT-mode HBA.
