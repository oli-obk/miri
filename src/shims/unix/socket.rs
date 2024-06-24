use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::io::{Error, ErrorKind, Read};
use std::rc::{Rc, Weak};

use crate::shims::unix::fd::WeakFileDescriptor;
use crate::shims::unix::*;
use crate::{concurrency::VClock, *};

use self::fd::FileDescriptor;
use self::shims::unix::linux::epoll::EpollEvent;

/// The maximum capacity of the socketpair buffer in bytes.
/// This number is arbitrary as the value can always
/// be configured in the real system.
const MAX_SOCKETPAIR_BUFFER_CAPACITY: usize = 212992;

/// Pair of connected sockets.
#[derive(Debug)]
struct SocketPair {
    // By making the write link weak, a `write` can detect when all readers are
    // gone, and trigger EPIPE as appropriate.
    writebuf: Weak<RefCell<Buffer>>,
    readbuf: Rc<RefCell<Buffer>>,
    peer_fd: WeakFileDescriptor,
    is_nonblock: bool,
    peer_closed: bool,
    // epoll_events is a list of epoll_event associated with this file description.
    // The key is (file descriptor value, epoll file descriptor value).
    // This will be correct when the same file description is inserted twice to an epoll instance
    // because their file descriptor values need to be different.
    // When a file description is inserted to two different epoll instance,
    // two different epoll_event will exist in epoll_events.
    epoll_events: BTreeMap<(i32, i32), Weak<EpollEvent>>,
}

#[derive(Debug)]
struct Buffer {
    buf: VecDeque<u8>,
    clock: VClock,
    /// Indicates if there is at least one active writer to this buffer.
    /// If all writers of this buffer are dropped, buf_has_writer becomes false and we
    /// indicate EOF instead of blocking.
    buf_has_writer: bool,
}

impl FileDescription for SocketPair {
    fn name(&self) -> &'static str {
        "socketpair"
    }

    fn get_epoll_events<'tcx>(
        &mut self,
    ) -> InterpResult<'tcx, &mut BTreeMap<(i32, i32), Weak<EpollEvent>>> {
        Ok(&mut self.epoll_events)
    }

    fn check_and_update_readiness<'tcx>(&self, ecx: &mut MiriInterpCx<'tcx>) -> InterpResult<'tcx> {
        // We only check the status of EPOLLIN, EPOLLOUT and EPOLLRDHUP flag. If other event flags
        // need to be supported in the future, the check should be added here.
        if self.epoll_events.is_empty() {
            return Ok(());
        }
        let epollin = ecx.eval_libc_u32("EPOLLIN");
        let epollout = ecx.eval_libc_u32("EPOLLOUT");
        let epollrdhup = ecx.eval_libc_u32("EPOLLRDHUP");
        let mut ready_flags = 0;
        let readbuf = self.readbuf.borrow();

        // Check if it is readable.
        if !readbuf.buf.is_empty() {
            ready_flags |= epollin;
        }

        // Check if is writable.
        if let Some(writebuf) = self.writebuf.upgrade() {
            let writebuf = writebuf.borrow();
            let data_size = writebuf.buf.len();
            let available_space = MAX_SOCKETPAIR_BUFFER_CAPACITY.strict_sub(data_size);
            if available_space != 0 {
                ready_flags |= epollout;
            }
        }

        // Check if the peer_fd closed
        if self.peer_closed {
            ready_flags |= epollrdhup;
            // This is an edge case. Whenever epollrdhup is triggered, epollin will be added
            // even though there is no data in the buffer.
            ready_flags |= epollin;
        }
        ecx.update_readiness(ready_flags, &self.epoll_events)?;
        Ok(())
    }

    fn close<'tcx>(
        self: Box<Self>,
        ecx: &mut MiriInterpCx<'tcx>,
        _communicate_allowed: bool,
    ) -> InterpResult<'tcx, io::Result<()>> {
        // This is used to signal socketfd of other side that there is no writer to its readbuf.
        // If the upgrade fails, there is no need to update as all read ends have been dropped.
        if let Some(writebuf) = self.writebuf.upgrade() {
            writebuf.borrow_mut().buf_has_writer = false;
        };

        // Notify peer fd that closed has happened.
        if let Some(peer_fd) = self.peer_fd.upgrade() {
            let mut binding = peer_fd.borrow_mut();
            let peer_socketpair = binding.downcast_mut::<SocketPair>().unwrap();
            peer_socketpair.peer_closed = true;
            // When any of the event happened, we check and update the status of all supported flags
            // of peer fd.
            peer_socketpair.check_and_update_readiness(ecx)?;
        }
        Ok(Ok(()))
    }

    fn read<'tcx>(
        &mut self,
        _communicate_allowed: bool,
        bytes: &mut [u8],
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx, io::Result<usize>> {
        let request_byte_size = bytes.len();
        let mut readbuf = self.readbuf.borrow_mut();

        // Always succeed on read size 0.
        if request_byte_size == 0 {
            return Ok(Ok(0));
        }

        if readbuf.buf.is_empty() {
            if !readbuf.buf_has_writer {
                // Socketpair with no writer and empty buffer.
                // 0 bytes successfully read indicates end-of-file.
                return Ok(Ok(0));
            } else {
                if self.is_nonblock {
                    // Non-blocking socketpair with writer and empty buffer.
                    // https://linux.die.net/man/2/read
                    // EAGAIN or EWOULDBLOCK can be returned for socket,
                    // POSIX.1-2001 allows either error to be returned for this case.
                    // Since there is no ErrorKind for EAGAIN, WouldBlock is used.
                    return Ok(Err(Error::from(ErrorKind::WouldBlock)));
                } else {
                    // Blocking socketpair with writer and empty buffer.
                    // FIXME: blocking is currently not supported
                    throw_unsup_format!("socketpair read: blocking isn't supported yet");
                }
            }
        }

        // Synchronize with all previous writes to this buffer.
        // FIXME: this over-synchronizes; a more precise approach would be to
        // only sync with the writes whose data we will read.
        ecx.acquire_clock(&readbuf.clock);
        // Do full read / partial read based on the space available.
        // Conveniently, `read` exists on `VecDeque` and has exactly the desired behavior.
        let actual_read_size = readbuf.buf.read(bytes).unwrap();
        // The readbuf needs to be explicitly dropped because it will cause panic when
        // check_and_update_readiness borrow it again.
        drop(readbuf);
        if let Some(peer_fd) = self.peer_fd.upgrade() {
            peer_fd
                .borrow_mut()
                .downcast_mut::<SocketPair>()
                .unwrap()
                .check_and_update_readiness(ecx)?;
        }
        return Ok(Ok(actual_read_size));
    }

    fn write<'tcx>(
        &mut self,
        _communicate_allowed: bool,
        bytes: &[u8],
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx, io::Result<usize>> {
        let write_size = bytes.len();
        // Always succeed on write size 0.
        // ("If count is zero and fd refers to a file other than a regular file, the results are not specified.")
        if write_size == 0 {
            return Ok(Ok(0));
        }

        let Some(writebuf) = self.writebuf.upgrade() else {
            // If the upgrade from Weak to Rc fails, it indicates that all read ends have been
            // closed.
            return Ok(Err(Error::from(ErrorKind::BrokenPipe)));
        };
        let mut writebuf = writebuf.borrow_mut();
        let data_size = writebuf.buf.len();
        let available_space = MAX_SOCKETPAIR_BUFFER_CAPACITY.strict_sub(data_size);
        if available_space == 0 {
            if self.is_nonblock {
                // Non-blocking socketpair with a full buffer.
                return Ok(Err(Error::from(ErrorKind::WouldBlock)));
            } else {
                // Blocking socketpair with a full buffer.
                throw_unsup_format!("socketpair write: blocking isn't supported yet");
            }
        }
        // Remember this clock so `read` can synchronize with us.
        if let Some(clock) = &ecx.release_clock() {
            writebuf.clock.join(clock);
        }
        // Do full write / partial write based on the space available.
        let actual_write_size = write_size.min(available_space);
        writebuf.buf.extend(&bytes[..actual_write_size]);

        // The writebuf needs to be explicitly dropped because it will cause panic when
        // check_and_update_readiness borrow it again.
        drop(writebuf);
        // Notification should be provided for peer fd as it became readable.
        if let Some(peer_fd) = self.peer_fd.upgrade() {
            peer_fd
                .borrow_mut()
                .downcast_mut::<SocketPair>()
                .unwrap()
                .check_and_update_readiness(ecx)?;
        }
        return Ok(Ok(actual_write_size));
    }
}

impl<'tcx> EvalContextExt<'tcx> for crate::MiriInterpCx<'tcx> {}
pub trait EvalContextExt<'tcx>: crate::MiriInterpCxExt<'tcx> {
    /// For more information on the arguments see the socketpair manpage:
    /// <https://linux.die.net/man/2/socketpair>
    fn socketpair(
        &mut self,
        domain: &OpTy<'tcx>,
        type_: &OpTy<'tcx>,
        protocol: &OpTy<'tcx>,
        sv: &OpTy<'tcx>,
    ) -> InterpResult<'tcx, Scalar> {
        let this = self.eval_context_mut();

        let domain = this.read_scalar(domain)?.to_i32()?;
        let mut type_ = this.read_scalar(type_)?.to_i32()?;
        let protocol = this.read_scalar(protocol)?.to_i32()?;
        let sv = this.deref_pointer(sv)?;

        let mut is_sock_nonblock = false;

        // Parse and remove the type flags that we support. If type != 0 after removing,
        // unsupported flags are used.
        if type_ & this.eval_libc_i32("SOCK_STREAM") == this.eval_libc_i32("SOCK_STREAM") {
            type_ &= !(this.eval_libc_i32("SOCK_STREAM"));
        }

        // SOCK_NONBLOCK only exists on Linux.
        if this.tcx.sess.target.os == "linux" {
            if type_ & this.eval_libc_i32("SOCK_NONBLOCK") == this.eval_libc_i32("SOCK_NONBLOCK") {
                is_sock_nonblock = true;
                type_ &= !(this.eval_libc_i32("SOCK_NONBLOCK"));
            }
            if type_ & this.eval_libc_i32("SOCK_CLOEXEC") == this.eval_libc_i32("SOCK_CLOEXEC") {
                type_ &= !(this.eval_libc_i32("SOCK_CLOEXEC"));
            }
        }

        // Fail on unsupported input.
        // AF_UNIX and AF_LOCAL are synonyms, so we accept both in case
        // their values differ.
        if domain != this.eval_libc_i32("AF_UNIX") && domain != this.eval_libc_i32("AF_LOCAL") {
            throw_unsup_format!(
                "socketpair: domain {:#x} is unsupported, only AF_UNIX \
                                 and AF_LOCAL are allowed",
                domain
            );
        } else if type_ != 0 {
            throw_unsup_format!(
                "socketpair: type {:#x} is unsupported, only SOCK_STREAM, \
                                 SOCK_CLOEXEC and SOCK_NONBLOCK are allowed",
                type_
            );
        } else if protocol != 0 {
            throw_unsup_format!(
                "socketpair: socket protocol {protocol} is unsupported, \
                                 only 0 is allowed",
            );
        }

        let buffer1 = Rc::new(RefCell::new(Buffer {
            buf: VecDeque::new(),
            clock: VClock::default(),
            buf_has_writer: true,
        }));

        let buffer2 = Rc::new(RefCell::new(Buffer {
            buf: VecDeque::new(),
            clock: VClock::default(),
            buf_has_writer: true,
        }));

        let socketpair_0 = SocketPair {
            writebuf: Rc::downgrade(&buffer1),
            readbuf: Rc::clone(&buffer2),
            peer_fd: WeakFileDescriptor::default(),
            peer_closed: false,
            is_nonblock: is_sock_nonblock,
            epoll_events: BTreeMap::new(),
        };
        let socketpair_1 = SocketPair {
            writebuf: Rc::downgrade(&buffer2),
            readbuf: Rc::clone(&buffer1),
            peer_fd: WeakFileDescriptor::default(),
            peer_closed: false,
            is_nonblock: is_sock_nonblock,
            epoll_events: BTreeMap::new(),
        };
        let file_descriptor0 = FileDescriptor::new(socketpair_0);
        let file_descriptor1 = FileDescriptor::new(socketpair_1);

        // Expose peer file descriptor to each other.
        let weak_file_descriptor0 = file_descriptor0.downgrade();
        file_descriptor1.borrow_mut().downcast_mut::<SocketPair>().unwrap().peer_fd =
            weak_file_descriptor0;
        let weak_file_descriptor1 = file_descriptor1.downgrade();
        file_descriptor0.borrow_mut().downcast_mut::<SocketPair>().unwrap().peer_fd =
            weak_file_descriptor1;

        let fds = &mut this.machine.fds;

        let sv0 = fds.insert_fd(file_descriptor0);
        let sv1 = fds.insert_fd(file_descriptor1);

        let sv0 = Scalar::from_int(sv0, sv.layout.size);
        let sv1 = Scalar::from_int(sv1, sv.layout.size);

        this.write_scalar(sv0, &sv)?;
        this.write_scalar(sv1, &sv.offset(sv.layout.size, sv.layout, this)?)?;

        Ok(Scalar::from_i32(0))
    }
}
