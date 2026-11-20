//! ublk520: present a disk with 520-byte sectors as a 512-byte-sector block device.
//!
//! Virtual 512-byte sector N  <->  first 512 bytes of physical 520-byte block N.
//! On write, the trailing 8 bytes of each physical block are written as zeros.
//! Only valid for drives with protection information (PI/DIF) DISABLED.
//!
//! Usage:
//!   sudo ublk520 [--id N] /dev/sg4        # real drive via SG_IO
//!   sudo ublk520 [--id N] --file test.img # test mode: regular file with 520-byte blocks
//! --id N requests /dev/ublkbN (default: first free).

use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::io::{BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{BufDesc, UblkError, UblkFlags, UblkIORes};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;

const VSEC: usize = 512; // virtual sector size
const CHUNK_BLOCKS: usize = 1024; // max physical blocks per SG_IO command (~520 KB)
const IO_BUF_BYTES: u32 = 512 * 1024; // max bytes per ublk request (1024 virtual sectors)
const NR_QUEUES: u16 = 32; // one per CPU; each queue handles one request at a time
const QUEUE_DEPTH: u16 = 8; // handler is synchronous, so extra depth only costs memory

// ---------------------------------------------------------------- SG_IO plumbing

#[repr(C)]
struct SgIoHdr {
    interface_id: libc::c_int,
    dxfer_direction: libc::c_int,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: libc::c_int,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: libc::c_int,
    duration: u32,
    info: u32,
}

const SG_IO: libc::c_ulong = 0x2285;
const SG_DXFER_NONE: i32 = -1;
const SG_DXFER_TO_DEV: i32 = -2;
const SG_DXFER_FROM_DEV: i32 = -3;

fn sg_cmd(fd: RawFd, cdb: &[u8], dir: i32, data: *mut u8, len: usize) -> io::Result<()> {
    let mut last_err = String::new();
    for _attempt in 0..4 {
        let mut sense = [0u8; 64];
        let mut cdb_copy = [0u8; 16];
        cdb_copy[..cdb.len()].copy_from_slice(cdb);
        let mut hdr: SgIoHdr = unsafe { std::mem::zeroed() };
        hdr.interface_id = b'S' as i32;
        hdr.dxfer_direction = dir;
        hdr.cmd_len = cdb.len() as u8;
        hdr.mx_sb_len = sense.len() as u8;
        hdr.dxfer_len = len as u32;
        hdr.dxferp = data as *mut libc::c_void;
        hdr.cmdp = cdb_copy.as_mut_ptr();
        hdr.sbp = sense.as_mut_ptr();
        hdr.timeout = 120_000;

        let r = unsafe { libc::ioctl(fd, SG_IO as _, &mut hdr as *mut SgIoHdr) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if hdr.info & 1 == 0 {
            return Ok(()); // SG_INFO_OK
        }
        let key = match sense[0] & 0x7f {
            0x70 | 0x71 => sense[2] & 0x0f,
            0x72 | 0x73 => sense[1] & 0x0f,
            _ => 0,
        };
        last_err = format!(
            "SCSI error: status={:#x} host={:#x} driver={:#x} sense={:02x?}",
            hdr.status,
            hdr.host_status,
            hdr.driver_status,
            &sense[..(hdr.sb_len_wr as usize).min(sense.len())]
        );
        if key == 0x6 {
            continue; // UNIT ATTENTION: retry
        }
        break;
    }
    Err(io::Error::new(io::ErrorKind::Other, last_err))
}

// ---------------------------------------------------------------- backends

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Sg,
    File,
}

#[derive(Clone, Copy)]
struct Backend {
    fd: RawFd,
    kind: Kind,
    bs: usize, // physical block size (520)
}

impl Backend {
    /// Returns (backend, number of physical blocks).
    fn probe(fd: RawFd, kind: Kind) -> io::Result<(Backend, u64)> {
        match kind {
            Kind::Sg => {
                let mut cdb = [0u8; 16];
                cdb[0] = 0x9e; // SERVICE ACTION IN(16)
                cdb[1] = 0x10; // READ CAPACITY(16)
                cdb[10..14].copy_from_slice(&32u32.to_be_bytes());
                let mut resp = [0u8; 32];
                sg_cmd(fd, &cdb, SG_DXFER_FROM_DEV, resp.as_mut_ptr(), resp.len())?;
                let last = u64::from_be_bytes(resp[0..8].try_into().unwrap());
                let bs = u32::from_be_bytes(resp[8..12].try_into().unwrap()) as usize;
                let prot_en = resp[12] & 1;
                if prot_en != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "drive has protection information (PI) enabled; refusing (writes would fail)",
                    ));
                }
                Ok((Backend { fd, kind, bs }, last + 1))
            }
            Kind::File => {
                let bs = 520usize;
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe { libc::fstat(fd, &mut st) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok((Backend { fd, kind, bs }, st.st_size as u64 / bs as u64))
            }
        }
    }

    fn read(&self, lba: u64, nblk: usize, buf: &mut [u8]) -> io::Result<()> {
        debug_assert!(buf.len() >= nblk * self.bs);
        match self.kind {
            Kind::Sg => {
                let mut cdb = [0u8; 16];
                cdb[0] = 0x88; // READ(16)
                cdb[2..10].copy_from_slice(&lba.to_be_bytes());
                cdb[10..14].copy_from_slice(&(nblk as u32).to_be_bytes());
                sg_cmd(self.fd, &cdb, SG_DXFER_FROM_DEV, buf.as_mut_ptr(), nblk * self.bs)
            }
            Kind::File => {
                let want = nblk * self.bs;
                let mut done = 0usize;
                while done < want {
                    let n = unsafe {
                        libc::pread(
                            self.fd,
                            buf[done..].as_mut_ptr() as *mut libc::c_void,
                            want - done,
                            (lba as usize * self.bs + done) as libc::off_t,
                        )
                    };
                    if n < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if n == 0 {
                        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                    }
                    done += n as usize;
                }
                Ok(())
            }
        }
    }

    fn write(&self, lba: u64, nblk: usize, buf: &[u8]) -> io::Result<()> {
        debug_assert!(buf.len() >= nblk * self.bs);
        match self.kind {
            Kind::Sg => {
                let mut cdb = [0u8; 16];
                cdb[0] = 0x8a; // WRITE(16)
                cdb[2..10].copy_from_slice(&lba.to_be_bytes());
                cdb[10..14].copy_from_slice(&(nblk as u32).to_be_bytes());
                sg_cmd(
                    self.fd,
                    &cdb,
                    SG_DXFER_TO_DEV,
                    buf.as_ptr() as *mut u8,
                    nblk * self.bs,
                )
            }
            Kind::File => {
                let want = nblk * self.bs;
                let mut done = 0usize;
                while done < want {
                    let n = unsafe {
                        libc::pwrite(
                            self.fd,
                            buf[done..].as_ptr() as *const libc::c_void,
                            want - done,
                            (lba as usize * self.bs + done) as libc::off_t,
                        )
                    };
                    if n < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    done += n as usize;
                }
                Ok(())
            }
        }
    }

    fn flush(&self) -> io::Result<()> {
        match self.kind {
            Kind::Sg => {
                let mut cdb = [0u8; 16];
                cdb[0] = 0x91; // SYNCHRONIZE CACHE(16), LBA 0 / 0 blocks = whole device
                sg_cmd(self.fd, &cdb, SG_DXFER_NONE, std::ptr::null_mut(), 0)
            }
            Kind::File => {
                if unsafe { libc::fsync(self.fd) } < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 512 <-> 520 translation

/// Read `nsec` virtual sectors starting at `lba` into `dst` (len >= nsec*512).
fn tr_read(be: &Backend, lba: u64, nsec: usize, dst: &mut [u8], scratch: &mut [u8]) -> io::Result<()> {
    let mut done = 0usize;
    while done < nsec {
        let n = (nsec - done).min(CHUNK_BLOCKS);
        be.read(lba + done as u64, n, &mut scratch[..n * be.bs])?;
        for i in 0..n {
            let s = &scratch[i * be.bs..i * be.bs + VSEC];
            dst[(done + i) * VSEC..(done + i + 1) * VSEC].copy_from_slice(s);
        }
        done += n;
    }
    Ok(())
}

/// Write `nsec` virtual sectors starting at `lba` from `src`; trailing bytes of each block = 0.
fn tr_write(be: &Backend, lba: u64, nsec: usize, src: &[u8], scratch: &mut [u8]) -> io::Result<()> {
    let mut done = 0usize;
    while done < nsec {
        let n = (nsec - done).min(CHUNK_BLOCKS);
        for i in 0..n {
            let blk = &mut scratch[i * be.bs..(i + 1) * be.bs];
            blk[..VSEC].copy_from_slice(&src[(done + i) * VSEC..(done + i + 1) * VSEC]);
            blk[VSEC..].fill(0);
        }
        be.write(lba + done as u64, n, &scratch[..n * be.bs])?;
        done += n;
    }
    Ok(())
}

// ---------------------------------------------------------------- ublk glue

fn block_signals() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

fn wait_for_signal() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        let mut sig = 0;
        libc::sigwait(&set, &mut sig);
    }
}

fn usage() -> ! {
    eprintln!("usage: ublk520 [--id N] <sg-device, e.g. /dev/sg4>\n       ublk520 [--id N] --file <image with 520-byte blocks>");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut kind = Kind::Sg;
    let mut dev_id: i32 = -1;
    let mut path: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--file" => kind = Kind::File,
            "--id" => {
                dev_id = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage());
            }
            s if !s.starts_with('-') && path.is_none() => path = Some(s.to_string()),
            _ => usage(),
        }
    }
    let path = path.unwrap_or_else(|| usage());

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(&path)
        .unwrap_or_else(|e| {
            eprintln!("cannot open {}: {}", path, e);
            std::process::exit(1)
        });
    let (be, nblocks) = Backend::probe(file.as_raw_fd(), kind).unwrap_or_else(|e| {
        eprintln!("probe failed: {}", e);
        std::process::exit(1)
    });
    if be.bs < VSEC {
        eprintln!("physical block size {} is smaller than 512; nothing to do", be.bs);
        std::process::exit(1);
    }
    let dev_size = nblocks * VSEC as u64;
    eprintln!(
        "{}: {} blocks of {} bytes -> exposing {} virtual 512-byte sectors ({:.2} GB)",
        path,
        nblocks,
        be.bs,
        nblocks,
        dev_size as f64 / 1e9
    );

    block_signals(); // queue threads inherit this; a dedicated thread handles the signal

    let ctrl = UblkCtrlBuilder::default()
        .name("ublk520")
        .id(dev_id)
        .nr_queues(NR_QUEUES)
        .depth(QUEUE_DEPTH)
        .io_buf_bytes(IO_BUF_BYTES)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
        .unwrap_or_else(|e| {
            eprintln!("cannot create ublk device (is ublk_drv loaded? running as root?): {}", e);
            std::process::exit(1)
        });

    let tgt_init = move |dev: &mut UblkDev| {
        dev.set_default_params(dev_size);
        dev.tgt.params.basic.physical_bs_shift = 9;
        dev.tgt.params.basic.io_opt_shift = 9;
        dev.tgt.params.basic.io_min_shift = 9;
        Ok(())
    };

    let q_fn = move |qid: u16, dev: &UblkDev| {
        let bufs_rc = Rc::new(dev.alloc_queue_io_bufs());
        let bufs = bufs_rc.clone();
        let mut scratch = vec![0u8; CHUNK_BLOCKS * be.bs];

        let io_handler = move |q: &UblkQueue, tag: u16, _io: &UblkIOCtx| {
            let iod = q.get_iod(tag);
            let op = iod.op_flags & 0xff;
            let lba = iod.start_sector;
            let nsec = iod.nr_sectors as usize;
            let ibuf = &bufs[tag as usize];

            let res: io::Result<()> = if op == libublk::sys::UBLK_IO_OP_READ {
                let dst = unsafe { std::slice::from_raw_parts_mut(ibuf.as_mut_ptr(), nsec * VSEC) };
                tr_read(&be, lba, nsec, dst, &mut scratch)
            } else if op == libublk::sys::UBLK_IO_OP_WRITE {
                let src = unsafe { std::slice::from_raw_parts(ibuf.as_ptr(), nsec * VSEC) };
                tr_write(&be, lba, nsec, src, &mut scratch)
            } else if op == libublk::sys::UBLK_IO_OP_FLUSH {
                be.flush()
            } else {
                Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))
            };

            let ret = match res {
                Ok(()) => Ok(UblkIORes::Result((nsec * VSEC) as i32)),
                Err(e) => {
                    eprintln!("I/O error (op {} lba {} nsec {}): {}", op, lba, nsec, e);
                    Err(UblkError::OtherError(-e.raw_os_error().unwrap_or(libc::EIO)))
                }
            };
            q.complete_io_cmd_unified(tag, BufDesc::Slice(ibuf.as_slice()), ret)
                .unwrap();
        };

        let queue = match UblkQueue::new(qid, dev)
            .unwrap()
            .submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs_rc)))
        {
            Ok(q) => q,
            Err(e) => {
                eprintln!("submit_fetch_commands failed: {}", e);
                return;
            }
        };
        queue.wait_and_handle_io(io_handler);
    };

    let wh = move |d_ctrl: &UblkCtrl| {
        let id = d_ctrl.dev_info().dev_id as i32;
        eprintln!("ready: /dev/ublkb{}  (Ctrl-C / SIGTERM to detach)", id);
        std::thread::spawn(move || {
            wait_for_signal();
            eprintln!("shutting down, flushing...");
            if let Ok(c) = UblkCtrl::new_simple(id) {
                let _ = c.kill_dev();
            }
        });
    };

    if let Err(e) = ctrl.run_target(tgt_init, q_fn, wh) {
        eprintln!("run_target failed: {}", e);
    }
    let _ = ctrl.del_dev();
    let _ = be.flush();
    drop(file);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(blocks: usize) -> (std::fs::File, Backend) {
        let path = format!("/tmp/ublk520-test-{}-{}.img", std::process::id(), blocks);
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        f.set_len((blocks * 520) as u64).unwrap();
        std::fs::remove_file(&path).unwrap();
        let (be, n) = Backend::probe(f.as_raw_fd(), Kind::File).unwrap();
        assert_eq!(n as usize, blocks);
        (f, be)
    }

    #[test]
    fn roundtrip_and_layout() {
        let blocks = 1000;
        let (f, be) = mk(blocks);
        let mut scratch = vec![0u8; CHUNK_BLOCKS * be.bs];
        // 300 sectors starting at lba 17: spans multiple chunks (128 blocks each)
        let nsec = 300usize;
        let src: Vec<u8> = (0..nsec * VSEC).map(|i| (i * 7 + 13) as u8).collect();
        tr_write(&be, 17, nsec, &src, &mut scratch).unwrap();
        let mut dst = vec![0u8; nsec * VSEC];
        tr_read(&be, 17, nsec, &mut dst, &mut scratch).unwrap();
        assert_eq!(src, dst);
        // physical layout: block N at N*520, data in first 512 bytes, 8-byte tail zero
        let mut raw = vec![0u8; 520];
        be.read(17 + 5, 1, &mut raw).unwrap();
        assert_eq!(&raw[..512], &src[5 * 512..6 * 512]);
        assert_eq!(&raw[512..], &[0u8; 8]);
        // neighbours untouched
        be.read(16, 1, &mut raw).unwrap();
        assert!(raw.iter().all(|&b| b == 0));
        be.read(17 + 300, 1, &mut raw).unwrap();
        assert!(raw.iter().all(|&b| b == 0));
        be.flush().unwrap();
        drop(f);
    }

    #[test]
    fn overwrite_clears_tail() {
        let (f, be) = mk(10);
        let mut scratch = vec![0u8; CHUNK_BLOCKS * be.bs];
        // dirty tail directly, then a virtual write must zero it
        let dirty = vec![0xAAu8; 520];
        be.write(3, 1, &dirty).unwrap();
        tr_write(&be, 3, 1, &vec![0x55u8; 512], &mut scratch).unwrap();
        let mut raw = vec![0u8; 520];
        be.read(3, 1, &mut raw).unwrap();
        assert!(raw[..512].iter().all(|&b| b == 0x55));
        assert!(raw[512..].iter().all(|&b| b == 0));
        drop(f);
    }

    #[test]
    fn sg_struct_size() {
        // sizeof(sg_io_hdr_t) is 88 on 64-bit Linux
        assert_eq!(std::mem::size_of::<SgIoHdr>(), 88);
    }
}
