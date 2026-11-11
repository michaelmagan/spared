#!/usr/bin/env python3
"""Page-cache tool (no vmtouch/fincore on this VM).
  c-pcache.py evict ROOT GLOB [GLOB...]     fsync + posix_fadvise(DONTNEED) on every file under ROOT whose basename matches a glob
  c-pcache.py resident ROOT GLOB [GLOB...]  resident/total MiB of the matching files via mincore(2)
Hardlinked files are one inode and counted once."""
import ctypes, fnmatch, mmap, os, sys
libc = ctypes.CDLL(None, use_errno=True)
libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
def files(root, globs):
    seen = set()
    for d, _, fs in os.walk(root):
        for f in fs:
            if any(fnmatch.fnmatch(f, g) for g in globs):
                p = os.path.join(d, f); st = os.stat(p)
                if (st.st_dev, st.st_ino) in seen or st.st_size == 0: continue
                seen.add((st.st_dev, st.st_ino)); yield p, st.st_size
def resident_pages(p, size):
    fd = os.open(p, os.O_RDONLY)
    try:
        m = mmap.mmap(fd, size, flags=mmap.MAP_PRIVATE, prot=mmap.PROT_READ | mmap.PROT_WRITE)
        addr = ctypes.addressof(ctypes.c_char.from_buffer(m))
        pages = (size + 4095) // 4096
        vec = (ctypes.c_ubyte * pages)()
        if libc.mincore(ctypes.c_void_p(addr), size, vec) != 0: raise OSError(ctypes.get_errno(), "mincore")
        n = sum(v & 1 for v in vec)
        del vec
        return n, pages, m
    finally: os.close(fd)
if __name__ == "__main__":
    mode, root, globs = sys.argv[1], sys.argv[2], sys.argv[3:]
    if mode == "evict":
        n = b = 0
        for p, s in files(root, globs):
            fd = os.open(p, os.O_RDONLY)
            try: os.fsync(fd); os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            finally: os.close(fd)
            n += 1; b += s
        print(f"evicted {n} files {b/2**20:.0f} MiB")
    else:
        tr = tp = 0
        for p, s in files(root, globs):
            r, t, m = resident_pages(p, s); tr += r; tp += t
            try: m.close()
            except BufferError: pass
        print(f"resident {tr*4096/2**20:.1f} of {tp*4096/2**20:.1f} MiB")
