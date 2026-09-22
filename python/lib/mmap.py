"""mmap for a platform without memory mappings.

The guest is WebAssembly: a process's memory is one linear memory that the kernel cannot map a file
into, so CPython's C mmap module cannot be built here. This module keeps its interface — the same
constants, the same class and methods, the same errors — over a copy of the mapped range:

  * The range is read into memory when the map is created.
  * A shared writable map (MAP_SHARED with PROT_WRITE, or ACCESS_WRITE) writes every change made
    through its methods straight back to the file, and writes the whole range back on flush(),
    close() and when a buffer taken from it (memoryview, readinto) is released.
  * A private map (MAP_PRIVATE, ACCESS_COPY) never writes back; a read-only map refuses changes.

What cannot be emulated: two maps of one file, or a map and the file, do not see each other's
changes until the writer's data reaches the file and the reader creates a new map; and a map
takes as much memory as its length. Programs that only read files through mmap (the common case:
parsers, hashing, pip's cache) behave as usual.
"""

import os

__all__ = ["mmap", "error"]

error = OSError

ACCESS_DEFAULT, ACCESS_READ, ACCESS_WRITE, ACCESS_COPY = 0, 1, 2, 3
PROT_READ, PROT_WRITE, PROT_EXEC = 1, 2, 4
MAP_SHARED, MAP_PRIVATE = 0x01, 0x02
MAP_ANONYMOUS = MAP_ANON = 0x20
MAP_DENYWRITE, MAP_EXECUTABLE, MAP_POPULATE, MAP_STACK = 0x0800, 0x1000, 0x8000, 0x20000
MADV_NORMAL, MADV_RANDOM, MADV_SEQUENTIAL, MADV_WILLNEED, MADV_DONTNEED = 0, 1, 2, 3, 4
MADV_FREE, MADV_REMOVE, MADV_DONTFORK, MADV_DOFORK = 8, 9, 10, 11
MADV_MERGEABLE, MADV_UNMERGEABLE, MADV_HUGEPAGE, MADV_NOHUGEPAGE = 12, 13, 14, 15
MADV_DONTDUMP, MADV_DODUMP, MADV_HWPOISON = 16, 17, 100
try:
    PAGESIZE = os.sysconf("SC_PAGE_SIZE")
except (AttributeError, OSError, ValueError):
    PAGESIZE = 65536
ALLOCATIONGRANULARITY = PAGESIZE


class mmap:
    """mmap(fileno, length[, flags[, prot[, access[, offset[, trackfd]]]]])

    Maps length bytes from the file specified by the file descriptor fileno, or an anonymous
    block when fileno is -1. See the module documentation for what "maps" means here."""

    def __init__(self, fileno, length, flags=MAP_SHARED, prot=PROT_WRITE | PROT_READ,
                 access=ACCESS_DEFAULT, offset=0, *, trackfd=True):
        if length < 0:
            raise OverflowError("memory mapped length must be positive")
        if offset < 0:
            raise OverflowError("memory mapped offset must be positive")
        if access != ACCESS_DEFAULT and (flags != MAP_SHARED or prot != PROT_WRITE | PROT_READ):
            raise ValueError("mmap can't specify both access and flags, prot.")
        if access == ACCESS_READ:
            flags, prot = MAP_SHARED, PROT_READ
        elif access == ACCESS_WRITE:
            flags, prot = MAP_SHARED, PROT_READ | PROT_WRITE
        elif access == ACCESS_COPY:
            flags, prot = MAP_PRIVATE, PROT_READ | PROT_WRITE
        elif access == ACCESS_DEFAULT:
            if not prot & PROT_READ:
                raise ValueError("mmap invalid access parameter.")
            access = ACCESS_WRITE if prot & PROT_WRITE and flags & MAP_SHARED else (
                ACCESS_COPY if prot & PROT_WRITE else ACCESS_READ)
        else:
            raise ValueError("mmap invalid access parameter.")
        self._access = access
        self._offset = offset
        self._pos = 0
        self._exports = 0
        self._closed = False
        self._fd = -1
        self._anonymous = fileno == -1 or bool(flags & MAP_ANONYMOUS)
        if self._anonymous:
            if length == 0:
                raise ValueError("cannot mmap an empty file") if fileno != -1 else OSError(22, "Invalid argument")
            self._data = bytearray(length)
            return
        size = os.fstat(fileno).st_size
        if length == 0:
            if size == 0:
                raise ValueError("cannot mmap an empty file")
            if offset >= size:
                raise ValueError("mmap offset is greater than file size")
            length = size - offset
        elif offset > size or size - offset < length:
            raise ValueError("mmap length is greater than file size")
        self._fd = os.dup(fileno) if trackfd else fileno
        self._trackfd = trackfd
        self._data = bytearray(os.pread(fileno, length, offset))
        if len(self._data) < length:  # a short read: the file shrank meanwhile
            self._data.extend(bytes(length - len(self._data)))

    # -- state helpers -------------------------------------------------------------------------
    def _check(self):
        if self._closed:
            raise ValueError("mmap closed or invalid")

    def _writable(self):
        self._check()
        if self._access == ACCESS_READ:
            raise TypeError("mmap can't modify a readonly memory map.")

    def _shared(self):
        return self._access == ACCESS_WRITE and not self._anonymous

    def _store(self, start, end):
        """Write [start, end) back to the file when this map is shared and writable."""
        if self._shared() and end > start:
            os.pwrite(self._fd, self._data[start:end], self._offset + start)

    # -- the mmap interface --------------------------------------------------------------------
    def close(self):
        if self._closed:
            return
        if self._exports:
            raise BufferError("cannot close exported pointers exist")
        self._store(0, len(self._data))
        if self._fd >= 0 and self._trackfd:
            os.close(self._fd)
        self._fd = -1
        self._closed = True
        self._data = bytearray()

    @property
    def closed(self):
        return self._closed

    def __enter__(self):
        self._check()
        return self

    def __exit__(self, *exc):
        self.close()

    def __del__(self):
        try:
            if not self._closed and not self._exports:
                self.close()
        except Exception:
            pass

    def __len__(self):
        self._check()
        return len(self._data)

    def __getitem__(self, index):
        self._check()
        if isinstance(index, slice):
            return bytes(self._data[index])
        return self._data[index]

    def __setitem__(self, index, value):
        self._writable()
        if isinstance(index, slice):
            start, stop, step = index.indices(len(self._data))
            value = memoryview(value).tobytes() if not isinstance(value, (bytes, bytearray)) else value
            count = len(range(start, stop, step))
            if len(value) != count:
                raise IndexError("mmap slice assignment is wrong size")
            self._data[index] = value
            if count:
                low, high = (start, stop) if step > 0 else (stop + 1, start + 1)
                self._store(low, high)
        else:
            if not isinstance(value, int):
                raise TypeError("mmap item value must be an int")
            if index < 0:
                index += len(self._data)
            if not 0 <= index < len(self._data):
                raise IndexError("mmap index out of range")
            self._data[index] = value
            self._store(index, index + 1)

    def __buffer__(self, flags):
        self._check()
        self._exports += 1
        view = memoryview(self._data)
        return view.toreadonly() if self._access == ACCESS_READ else view

    def __release_buffer__(self, view):
        view.release()
        self._exports -= 1
        # Anything may have been written through the buffer.
        if not self._closed and self._access != ACCESS_READ:
            self._store(0, len(self._data))

    def find(self, sub, start=None, end=None):
        self._check()
        return self._data.find(sub, *_bounds(start, end, self._pos))

    def rfind(self, sub, start=None, end=None):
        self._check()
        return self._data.rfind(sub, *_bounds(start, end, self._pos))

    def flush(self, offset=0, size=None):
        self._check()
        if size is None:
            size = len(self._data) - offset
        if offset < 0 or size < 0 or offset + size > len(self._data):
            raise ValueError("flush values out of range")
        if self._access in (ACCESS_READ, ACCESS_COPY):
            return None
        self._store(offset, offset + size)
        if self._shared():
            os.fsync(self._fd)
        return None

    def madvise(self, option, start=0, length=None):
        self._check()
        if start < 0 or start >= len(self._data):
            raise ValueError("madvise start out of bounds")
        if length is not None and length < 0:
            raise ValueError("madvise length invalid")
        return None  # advice only; there is nothing to page in or out

    def move(self, dest, src, count):
        self._writable()
        size = len(self._data)
        if min(dest, src, count) < 0 or src + count > size or dest + count > size:
            raise ValueError("source, destination, or count out of range")
        self._data[dest:dest + count] = self._data[src:src + count]
        self._store(dest, dest + count)

    def read(self, n=None):
        self._check()
        remaining = max(len(self._data) - self._pos, 0)
        n = remaining if n is None or n < 0 else min(n, remaining)
        data = bytes(self._data[self._pos:self._pos + n])
        self._pos += n
        return data

    def read_byte(self):
        self._check()
        if self._pos >= len(self._data):
            raise ValueError("read byte out of range")
        value = self._data[self._pos]
        self._pos += 1
        return value

    def readline(self):
        self._check()
        end = self._data.find(b"\n", self._pos)
        end = len(self._data) if end < 0 else end + 1
        data = bytes(self._data[self._pos:end])
        self._pos = end
        return data

    def resize(self, newsize):
        self._writable()
        if self._access == ACCESS_COPY:
            raise TypeError("mmap can't resize a readonly or copy-on-write memory map.")
        if self._exports:
            raise BufferError("mmap can't resize with extant buffers exported.")
        if newsize < 0:
            raise ValueError("new size out of range")
        if not self._anonymous:
            os.ftruncate(self._fd, self._offset + newsize)
        if newsize < len(self._data):
            del self._data[newsize:]
        else:
            self._data.extend(bytes(newsize - len(self._data)))
        self._pos = min(self._pos, newsize)

    def seek(self, pos, whence=os.SEEK_SET):
        self._check()
        base = {os.SEEK_SET: 0, os.SEEK_CUR: self._pos, os.SEEK_END: len(self._data)}.get(whence)
        if base is None:
            raise ValueError("unknown seek type")
        if not 0 <= base + pos <= len(self._data):
            raise ValueError("seek out of range")
        self._pos = base + pos
        return self._pos

    def seekable(self):
        return True

    def size(self):
        self._check()
        if self._anonymous:  # as in C: an anonymous map has no file to ask
            raise OSError(9, os.strerror(9))
        return os.fstat(self._fd).st_size

    def tell(self):
        self._check()
        return self._pos

    def write(self, data):
        self._writable()
        data = memoryview(data).tobytes()
        if self._pos + len(data) > len(self._data):
            raise ValueError("data out of range")
        start = self._pos
        self._data[start:start + len(data)] = data
        self._pos += len(data)
        self._store(start, self._pos)
        return len(data)

    def write_byte(self, byte):
        self._writable()
        if self._pos >= len(self._data):
            raise ValueError("write byte out of range")
        self._data[self._pos] = byte
        self._pos += 1
        self._store(self._pos - 1, self._pos)

    def __repr__(self):
        name = {ACCESS_READ: "ACCESS_READ", ACCESS_WRITE: "ACCESS_WRITE",
                ACCESS_COPY: "ACCESS_COPY"}.get(self._access, "ACCESS_DEFAULT")
        if self._closed:
            return "<mmap.mmap closed=True>"
        return (f"<mmap.mmap closed=False, access={name}, length={len(self._data)}, "
                f"pos={self._pos}, offset={self._offset}>")


def _bounds(start, end, pos):
    return (pos if start is None else start, end) if end is not None else ((pos if start is None else start),)
