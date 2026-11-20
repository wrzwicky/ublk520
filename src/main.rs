//! ublk520: present a disk with 520-byte sectors as a 512-byte-sector block device.
//!
//! Virtual 512-byte sector N  <->  first 512 bytes of physical 520-byte block N.
//! On write, the trailing 8 bytes of each physical block are written as zeros.
//! Only valid for drives with protection information (PI/DIF) DISABLED.
//!
//! Usage:
//!   sudo ublk520 [--id N] /dev/sg4          # real drive via SG_IO
//!   sudo ublk520 [--id N] --file test.img   # test mode: regular file with 520-byte blocks
//! --id N requests /dev/ublkbN (default: first free).

use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::io::{BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{BufDesc, UblkError, UblkFlags, UblkIORes};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;

// ---------------------------------------------------------------- tunables

/// Sector size of the block device we present to the kernel.
const VIRTUAL_SECTOR_BYTES: usize = 512;
/// Upper bound on physical blocks carried by one SCSI READ/WRITE command (~520 KB).
/// A request never waits for or pads to this; it only splits larger requests.
const MAX_BLOCKS_PER_SCSI_COMMAND: usize = 1024;
/// Largest single request the kernel may send us (1024 virtual sectors = one SCSI command).
const UBLK_MAX_REQUEST_BYTES: u32 = 512 * 1024;
/// One ublk queue per CPU. Each queue handles one request at a time, so parallelism comes from queues.
const UBLK_QUEUE_COUNT: u16 = 32;
/// Requests a queue may have outstanding. Handling is synchronous, so more only costs memory.
const UBLK_QUEUE_DEPTH: u16 = 8;
/// How long a single SCSI command may take before SG_IO gives up.
const SCSI_COMMAND_TIMEOUT_MS: u32 = 120_000;
/// Retries for a command that fails with UNIT ATTENTION (e.g. after a bus reset).
const SCSI_UNIT_ATTENTION_RETRIES: usize = 4;

// ---------------------------------------------------------------- SG_IO plumbing
// Names below mirror <scsi/sg.h> so they can be checked against the kernel header.

/// Layout of `struct sg_io_hdr` from <scsi/sg.h>.
#[repr(C)]
struct SgIoHeader {
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

const SG_IO_IOCTL: libc::c_ulong = 0x2285;
const SG_DXFER_NONE: i32 = -1;
const SG_DXFER_TO_DEV: i32 = -2;
const SG_DXFER_FROM_DEV: i32 = -3;
/// `info & SG_INFO_CHECK` is zero when the command completed without error.
const SG_INFO_CHECK: u32 = 0x1;
const SENSE_KEY_UNIT_ATTENTION: u8 = 0x6;

/// Send one SCSI command through the sg driver and wait for it to finish.
/// Retries on EINTR and on UNIT ATTENTION; any other failure is returned with the sense bytes.
fn submit_scsi_command(
    device_fd: RawFd,
    command_block: &[u8],
    transfer_direction: i32,
    data: *mut u8,
    data_len: usize,
) -> io::Result<()> {
    let mut last_error_text = String::new();
    for _attempt in 0..SCSI_UNIT_ATTENTION_RETRIES {
        let mut sense_buffer = [0u8; 64];
        let mut command_block_copy = [0u8; 16];
        command_block_copy[..command_block.len()].copy_from_slice(command_block);

        let mut header: SgIoHeader = unsafe { std::mem::zeroed() };
        header.interface_id = b'S' as i32;
        header.dxfer_direction = transfer_direction;
        header.cmd_len = command_block.len() as u8;
        header.mx_sb_len = sense_buffer.len() as u8;
        header.dxfer_len = data_len as u32;
        header.dxferp = data as *mut libc::c_void;
        header.cmdp = command_block_copy.as_mut_ptr();
        header.sbp = sense_buffer.as_mut_ptr();
        header.timeout = SCSI_COMMAND_TIMEOUT_MS;

        let ioctl_result =
            unsafe { libc::ioctl(device_fd, SG_IO_IOCTL as _, &mut header as *mut SgIoHeader) };
        if ioctl_result < 0 {
            let os_error = io::Error::last_os_error();
            if os_error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(os_error);
        }
        if header.info & SG_INFO_CHECK == 0 {
            return Ok(());
        }

        // Sense data is "fixed" format (0x70/0x71, key in byte 2) or "descriptor" (0x72/0x73, key in byte 1).
        let sense_key = match sense_buffer[0] & 0x7f {
            0x70 | 0x71 => sense_buffer[2] & 0x0f,
            0x72 | 0x73 => sense_buffer[1] & 0x0f,
            _ => 0,
        };
        last_error_text = format!(
            "SCSI error: status={:#x} host={:#x} driver={:#x} sense={:02x?}",
            header.status,
            header.host_status,
            header.driver_status,
            &sense_buffer[..(header.sb_len_wr as usize).min(sense_buffer.len())]
        );
        if sense_key == SENSE_KEY_UNIT_ATTENTION {
            continue;
        }
        break;
    }
    Err(io::Error::new(io::ErrorKind::Other, last_error_text))
}

// ---------------------------------------------------------------- physical disk access

/// How we reach the physical blocks.
#[derive(Clone, Copy, PartialEq)]
enum DiskAccess {
    /// A real drive through a /dev/sgN node, using SCSI READ(16)/WRITE(16).
    ScsiGeneric,
    /// A regular file laid out as consecutive 520-byte blocks (for testing without hardware).
    RegularFile,
}

/// A disk addressed in physical blocks (520 bytes each), as the drive itself sees them.
#[derive(Clone, Copy)]
struct PhysicalDisk {
    device_fd: RawFd,
    access: DiskAccess,
    /// Physical block size in bytes (520 for the drives this tool exists for).
    block_bytes: usize,
}

impl PhysicalDisk {
    /// Ask the drive (or inspect the file) for its block size and block count.
    /// Returns the disk and its total number of physical blocks.
    fn identify(device_fd: RawFd, access: DiskAccess) -> io::Result<(PhysicalDisk, u64)> {
        match access {
            DiskAccess::ScsiGeneric => {
                // READ CAPACITY(16): SERVICE ACTION IN(16) opcode 0x9e, service action 0x10.
                let mut command_block = [0u8; 16];
                command_block[0] = 0x9e;
                command_block[1] = 0x10;
                command_block[10..14].copy_from_slice(&32u32.to_be_bytes());
                let mut response = [0u8; 32];
                submit_scsi_command(
                    device_fd,
                    &command_block,
                    SG_DXFER_FROM_DEV,
                    response.as_mut_ptr(),
                    response.len(),
                )?;
                let last_block_address = u64::from_be_bytes(response[0..8].try_into().unwrap());
                let block_bytes = u32::from_be_bytes(response[8..12].try_into().unwrap()) as usize;
                let protection_enabled = response[12] & 1 != 0;
                if protection_enabled {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "drive has protection information (PI) enabled; refusing (writes would fail)",
                    ));
                }
                Ok((
                    PhysicalDisk { device_fd, access, block_bytes },
                    last_block_address + 1,
                ))
            }
            DiskAccess::RegularFile => {
                let block_bytes = 520usize;
                let mut file_status: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe { libc::fstat(device_fd, &mut file_status) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                let block_count = file_status.st_size as u64 / block_bytes as u64;
                Ok((PhysicalDisk { device_fd, access, block_bytes }, block_count))
            }
        }
    }

    /// Read `block_count` physical blocks starting at `first_block` into `destination`.
    fn read_blocks(&self, first_block: u64, block_count: usize, destination: &mut [u8]) -> io::Result<()> {
        let byte_count = block_count * self.block_bytes;
        debug_assert!(destination.len() >= byte_count);
        match self.access {
            DiskAccess::ScsiGeneric => {
                let mut command_block = [0u8; 16];
                command_block[0] = 0x88; // READ(16)
                command_block[2..10].copy_from_slice(&first_block.to_be_bytes());
                command_block[10..14].copy_from_slice(&(block_count as u32).to_be_bytes());
                submit_scsi_command(
                    self.device_fd,
                    &command_block,
                    SG_DXFER_FROM_DEV,
                    destination.as_mut_ptr(),
                    byte_count,
                )
            }
            DiskAccess::RegularFile => {
                let mut bytes_done = 0usize;
                while bytes_done < byte_count {
                    let bytes_read = unsafe {
                        libc::pread(
                            self.device_fd,
                            destination[bytes_done..].as_mut_ptr() as *mut libc::c_void,
                            byte_count - bytes_done,
                            (first_block as usize * self.block_bytes + bytes_done) as libc::off_t,
                        )
                    };
                    if bytes_read < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if bytes_read == 0 {
                        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                    }
                    bytes_done += bytes_read as usize;
                }
                Ok(())
            }
        }
    }

    /// Write `block_count` physical blocks starting at `first_block` from `source`.
    fn write_blocks(&self, first_block: u64, block_count: usize, source: &[u8]) -> io::Result<()> {
        let byte_count = block_count * self.block_bytes;
        debug_assert!(source.len() >= byte_count);
        match self.access {
            DiskAccess::ScsiGeneric => {
                let mut command_block = [0u8; 16];
                command_block[0] = 0x8a; // WRITE(16)
                command_block[2..10].copy_from_slice(&first_block.to_be_bytes());
                command_block[10..14].copy_from_slice(&(block_count as u32).to_be_bytes());
                submit_scsi_command(
                    self.device_fd,
                    &command_block,
                    SG_DXFER_TO_DEV,
                    source.as_ptr() as *mut u8, // the kernel only reads this buffer for a write
                    byte_count,
                )
            }
            DiskAccess::RegularFile => {
                let mut bytes_done = 0usize;
                while bytes_done < byte_count {
                    let bytes_written = unsafe {
                        libc::pwrite(
                            self.device_fd,
                            source[bytes_done..].as_ptr() as *const libc::c_void,
                            byte_count - bytes_done,
                            (first_block as usize * self.block_bytes + bytes_done) as libc::off_t,
                        )
                    };
                    if bytes_written < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    bytes_done += bytes_written as usize;
                }
                Ok(())
            }
        }
    }

    /// Make the drive commit its volatile write cache to stable storage.
    fn flush_cache(&self) -> io::Result<()> {
        match self.access {
            DiskAccess::ScsiGeneric => {
                // SYNCHRONIZE CACHE(16) with LBA 0 and 0 blocks means the whole device.
                let mut command_block = [0u8; 16];
                command_block[0] = 0x91;
                submit_scsi_command(self.device_fd, &command_block, SG_DXFER_NONE, std::ptr::null_mut(), 0)
            }
            DiskAccess::RegularFile => {
                if unsafe { libc::fsync(self.device_fd) } < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 512 <-> 520 translation

/// Read `sector_count` virtual 512-byte sectors starting at `first_sector` into `destination`
/// (which must hold at least sector_count * 512 bytes). `staging_buffer` must hold
/// MAX_BLOCKS_PER_SCSI_COMMAND physical blocks.
fn read_virtual_sectors(
    disk: &PhysicalDisk,
    first_sector: u64,
    sector_count: usize,
    destination: &mut [u8],
    staging_buffer: &mut [u8],
) -> io::Result<()> {
    let mut sectors_done = 0usize;
    while sectors_done < sector_count {
        let batch = (sector_count - sectors_done).min(MAX_BLOCKS_PER_SCSI_COMMAND);
        disk.read_blocks(first_sector + sectors_done as u64, batch, &mut staging_buffer[..batch * disk.block_bytes])?;
        for i in 0..batch {
            // Keep the first 512 bytes of each physical block; drop the tail.
            let physical_block = &staging_buffer[i * disk.block_bytes..i * disk.block_bytes + VIRTUAL_SECTOR_BYTES];
            let slot = (sectors_done + i) * VIRTUAL_SECTOR_BYTES;
            destination[slot..slot + VIRTUAL_SECTOR_BYTES].copy_from_slice(physical_block);
        }
        sectors_done += batch;
    }
    Ok(())
}

/// Write `sector_count` virtual 512-byte sectors starting at `first_sector` from `source`.
/// Each physical block gets the 512 data bytes followed by zeros for the remaining tail bytes.
fn write_virtual_sectors(
    disk: &PhysicalDisk,
    first_sector: u64,
    sector_count: usize,
    source: &[u8],
    staging_buffer: &mut [u8],
) -> io::Result<()> {
    let mut sectors_done = 0usize;
    while sectors_done < sector_count {
        let batch = (sector_count - sectors_done).min(MAX_BLOCKS_PER_SCSI_COMMAND);
        for i in 0..batch {
            let physical_block = &mut staging_buffer[i * disk.block_bytes..(i + 1) * disk.block_bytes];
            let slot = (sectors_done + i) * VIRTUAL_SECTOR_BYTES;
            physical_block[..VIRTUAL_SECTOR_BYTES].copy_from_slice(&source[slot..slot + VIRTUAL_SECTOR_BYTES]);
            physical_block[VIRTUAL_SECTOR_BYTES..].fill(0);
        }
        disk.write_blocks(first_sector + sectors_done as u64, batch, &staging_buffer[..batch * disk.block_bytes])?;
        sectors_done += batch;
    }
    Ok(())
}

// ---------------------------------------------------------------- shutdown signal handling

/// Block SIGINT/SIGTERM in the calling thread. Called before any other threads are created,
/// so every thread inherits the mask and only `wait_for_shutdown_signal` ever sees them.
fn block_shutdown_signals_in_this_thread() {
    unsafe {
        let mut signal_set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut signal_set);
        libc::sigaddset(&mut signal_set, libc::SIGINT);
        libc::sigaddset(&mut signal_set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &signal_set, std::ptr::null_mut());
    }
}

/// Block until SIGINT or SIGTERM arrives.
fn wait_for_shutdown_signal() {
    unsafe {
        let mut signal_set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut signal_set);
        libc::sigaddset(&mut signal_set, libc::SIGINT);
        libc::sigaddset(&mut signal_set, libc::SIGTERM);
        let mut received_signal = 0;
        libc::sigwait(&signal_set, &mut received_signal);
    }
}

// ---------------------------------------------------------------- command line and ublk glue

fn print_usage_and_exit() -> ! {
    eprintln!(
        "usage: ublk520 [--id N] <sg-device, e.g. /dev/sg4>\n       ublk520 [--id N] --file <image with 520-byte blocks>"
    );
    std::process::exit(2);
}

fn main() {
    let command_line: Vec<String> = std::env::args().skip(1).collect();
    let mut access = DiskAccess::ScsiGeneric;
    let mut requested_ublk_device_id: i32 = -1; // -1 = let the kernel pick
    let mut device_path: Option<String> = None;
    let mut arguments = command_line.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--file" => access = DiskAccess::RegularFile,
            "--id" => {
                requested_ublk_device_id = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_else(|| print_usage_and_exit());
            }
            path if !path.starts_with('-') && device_path.is_none() => device_path = Some(path.to_string()),
            _ => print_usage_and_exit(),
        }
    }
    let device_path = device_path.unwrap_or_else(|| print_usage_and_exit());

    let device_file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(&device_path)
        .unwrap_or_else(|error| {
            eprintln!("cannot open {}: {}", device_path, error);
            std::process::exit(1)
        });
    let (disk, physical_block_count) = PhysicalDisk::identify(device_file.as_raw_fd(), access)
        .unwrap_or_else(|error| {
            eprintln!("cannot identify disk: {}", error);
            std::process::exit(1)
        });
    if disk.block_bytes < VIRTUAL_SECTOR_BYTES {
        eprintln!("physical block size {} is smaller than 512; nothing to do", disk.block_bytes);
        std::process::exit(1);
    }
    let virtual_capacity_bytes = physical_block_count * VIRTUAL_SECTOR_BYTES as u64;
    eprintln!(
        "{}: {} blocks of {} bytes -> exposing {} virtual 512-byte sectors ({:.2} GB)",
        device_path,
        physical_block_count,
        disk.block_bytes,
        physical_block_count,
        virtual_capacity_bytes as f64 / 1e9
    );

    block_shutdown_signals_in_this_thread();

    let ublk_controller = UblkCtrlBuilder::default()
        .name("ublk520")
        .id(requested_ublk_device_id)
        .nr_queues(UBLK_QUEUE_COUNT)
        .depth(UBLK_QUEUE_DEPTH)
        .io_buf_bytes(UBLK_MAX_REQUEST_BYTES)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
        .unwrap_or_else(|error| {
            eprintln!("cannot create ublk device (is ublk_drv loaded? running as root?): {}", error);
            std::process::exit(1)
        });

    // Describe the block device to the kernel: 512-byte sectors, our capacity, volatile cache (so it sends flushes).
    let configure_ublk_target = move |ublk_device: &mut UblkDev| {
        ublk_device.set_default_params(virtual_capacity_bytes);
        ublk_device.tgt.params.basic.physical_bs_shift = 9;
        ublk_device.tgt.params.basic.io_opt_shift = 9;
        ublk_device.tgt.params.basic.io_min_shift = 9;
        Ok(())
    };

    // Runs on each queue's own thread: allocate buffers, then serve requests until the device is stopped.
    let serve_queue = move |queue_id: u16, ublk_device: &UblkDev| {
        let request_buffers_shared = Rc::new(ublk_device.alloc_queue_io_bufs());
        let request_buffers = request_buffers_shared.clone();
        let mut staging_buffer = vec![0u8; MAX_BLOCKS_PER_SCSI_COMMAND * disk.block_bytes];

        let handle_ublk_request = move |queue: &UblkQueue, tag: u16, _completion_context: &UblkIOCtx| {
            let request = queue.get_iod(tag);
            let operation = request.op_flags & 0xff;
            let first_sector = request.start_sector;
            let sector_count = request.nr_sectors as usize;
            let request_buffer = &request_buffers[tag as usize];

            let io_result: io::Result<()> = if operation == libublk::sys::UBLK_IO_OP_READ {
                let destination = unsafe {
                    std::slice::from_raw_parts_mut(request_buffer.as_mut_ptr(), sector_count * VIRTUAL_SECTOR_BYTES)
                };
                read_virtual_sectors(&disk, first_sector, sector_count, destination, &mut staging_buffer)
            } else if operation == libublk::sys::UBLK_IO_OP_WRITE {
                let source = unsafe {
                    std::slice::from_raw_parts(request_buffer.as_ptr(), sector_count * VIRTUAL_SECTOR_BYTES)
                };
                write_virtual_sectors(&disk, first_sector, sector_count, source, &mut staging_buffer)
            } else if operation == libublk::sys::UBLK_IO_OP_FLUSH {
                disk.flush_cache()
            } else {
                Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))
            };

            let completion = match io_result {
                Ok(()) => Ok(UblkIORes::Result((sector_count * VIRTUAL_SECTOR_BYTES) as i32)),
                Err(error) => {
                    eprintln!(
                        "I/O error (op {} first sector {} sectors {}): {}",
                        operation, first_sector, sector_count, error
                    );
                    Err(UblkError::OtherError(-error.raw_os_error().unwrap_or(libc::EIO)))
                }
            };
            queue
                .complete_io_cmd_unified(tag, BufDesc::Slice(request_buffer.as_slice()), completion)
                .unwrap();
        };

        let queue = match UblkQueue::new(queue_id, ublk_device)
            .unwrap()
            .submit_fetch_commands_unified(BufDescList::Slices(Some(&request_buffers_shared)))
        {
            Ok(queue) => queue,
            Err(error) => {
                eprintln!("submit_fetch_commands failed: {}", error);
                return;
            }
        };
        queue.wait_and_handle_io(handle_ublk_request);
    };

    // Called once the block device exists. Starts a thread that turns SIGINT/SIGTERM into a clean device stop.
    let on_device_started = move |controller: &UblkCtrl| {
        let ublk_device_id = controller.dev_info().dev_id as i32;
        eprintln!("ready: /dev/ublkb{}  (Ctrl-C / SIGTERM to detach)", ublk_device_id);
        std::thread::spawn(move || {
            wait_for_shutdown_signal();
            eprintln!("shutting down, flushing...");
            if let Ok(stop_controller) = UblkCtrl::new_simple(ublk_device_id) {
                let _ = stop_controller.kill_dev();
            }
        });
    };

    if let Err(error) = ublk_controller.run_target(configure_ublk_target, serve_queue, on_device_started) {
        eprintln!("run_target failed: {}", error);
    }
    let _ = ublk_controller.del_dev();
    let _ = disk.flush_cache();
    drop(device_file);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file-backed fake 520-byte-sector disk with `block_count` blocks. The file is
    /// unlinked immediately; the returned handle keeps it alive.
    fn make_fake_520_byte_disk(block_count: usize) -> (std::fs::File, PhysicalDisk) {
        let path = format!("/tmp/ublk520-test-{}-{}.img", std::process::id(), block_count);
        let fake_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        fake_file.set_len((block_count * 520) as u64).unwrap();
        std::fs::remove_file(&path).unwrap();
        let (disk, identified_block_count) =
            PhysicalDisk::identify(fake_file.as_raw_fd(), DiskAccess::RegularFile).unwrap();
        assert_eq!(identified_block_count as usize, block_count);
        (fake_file, disk)
    }

    #[test]
    fn virtual_write_then_read_roundtrips_across_command_boundaries() {
        let (_fake_file, disk) = make_fake_520_byte_disk(1000);
        let mut staging_buffer = vec![0u8; MAX_BLOCKS_PER_SCSI_COMMAND * disk.block_bytes];
        // A mid-sized request at a non-zero offset (single SCSI command; splitting is tested separately).
        let sector_count = 300usize;
        let first_sector = 17u64;
        let written: Vec<u8> = (0..sector_count * VIRTUAL_SECTOR_BYTES).map(|i| (i * 7 + 13) as u8).collect();
        write_virtual_sectors(&disk, first_sector, sector_count, &written, &mut staging_buffer).unwrap();

        let mut read_back = vec![0u8; sector_count * VIRTUAL_SECTOR_BYTES];
        read_virtual_sectors(&disk, first_sector, sector_count, &mut read_back, &mut staging_buffer).unwrap();
        assert_eq!(written, read_back);
    }

    #[test]
    fn physical_layout_has_data_then_zero_tail_and_neighbours_untouched() {
        let (_fake_file, disk) = make_fake_520_byte_disk(1000);
        let mut staging_buffer = vec![0u8; MAX_BLOCKS_PER_SCSI_COMMAND * disk.block_bytes];
        let written: Vec<u8> = (0..300 * VIRTUAL_SECTOR_BYTES).map(|i| (i * 7 + 13) as u8).collect();
        write_virtual_sectors(&disk, 17, 300, &written, &mut staging_buffer).unwrap();

        let mut physical_block = vec![0u8; 520];
        disk.read_blocks(17 + 5, 1, &mut physical_block).unwrap();
        assert_eq!(&physical_block[..512], &written[5 * 512..6 * 512]);
        assert_eq!(&physical_block[512..], &[0u8; 8]);

        disk.read_blocks(16, 1, &mut physical_block).unwrap(); // block before the range
        assert!(physical_block.iter().all(|&byte| byte == 0));
        disk.read_blocks(17 + 300, 1, &mut physical_block).unwrap(); // block after the range
        assert!(physical_block.iter().all(|&byte| byte == 0));
        disk.flush_cache().unwrap();
    }

    #[test]
    fn requests_larger_than_one_scsi_command_are_split_correctly() {
        // Needs more blocks than MAX_BLOCKS_PER_SCSI_COMMAND so the batching loop runs more than once.
        let block_count = MAX_BLOCKS_PER_SCSI_COMMAND * 2 + 37;
        let (_fake_file, disk) = make_fake_520_byte_disk(block_count);
        let mut staging_buffer = vec![0u8; MAX_BLOCKS_PER_SCSI_COMMAND * disk.block_bytes];
        let written: Vec<u8> = (0..block_count * VIRTUAL_SECTOR_BYTES).map(|i| (i * 31 + 5) as u8).collect();
        write_virtual_sectors(&disk, 0, block_count, &written, &mut staging_buffer).unwrap();
        let mut read_back = vec![0u8; block_count * VIRTUAL_SECTOR_BYTES];
        read_virtual_sectors(&disk, 0, block_count, &mut read_back, &mut staging_buffer).unwrap();
        assert_eq!(written, read_back);
    }

    #[test]
    fn overwriting_a_block_clears_a_dirty_tail() {
        let (_fake_file, disk) = make_fake_520_byte_disk(10);
        let mut staging_buffer = vec![0u8; MAX_BLOCKS_PER_SCSI_COMMAND * disk.block_bytes];
        // Put junk in the tail directly, then a virtual write must zero it.
        disk.write_blocks(3, 1, &vec![0xAAu8; 520]).unwrap();
        write_virtual_sectors(&disk, 3, 1, &vec![0x55u8; 512], &mut staging_buffer).unwrap();
        let mut physical_block = vec![0u8; 520];
        disk.read_blocks(3, 1, &mut physical_block).unwrap();
        assert!(physical_block[..512].iter().all(|&byte| byte == 0x55));
        assert!(physical_block[512..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn sg_io_header_matches_kernel_struct_size() {
        // sizeof(struct sg_io_hdr) is 88 on 64-bit Linux.
        assert_eq!(std::mem::size_of::<SgIoHeader>(), 88);
    }
}
