"""Linux mount identities for cleanup probes (requires Linux 6.8 or newer)."""
import ctypes
import os


class Statx(ctypes.Structure):
    # Linux's 256-byte statx ABI keeps stx_mnt_id at offset 0x90.
    _fields_ = [
        ("mask", ctypes.c_uint32),
        ("before_mount_id", ctypes.c_ubyte * 140),
        ("mount_id", ctypes.c_uint64),
        ("remaining", ctypes.c_ubyte * 104),
    ]


assert ctypes.sizeof(Statx) == 256 and Statx.mount_id.offset == 0x90
libc = ctypes.CDLL(None, use_errno=True)
libc.statx.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int,
                      ctypes.c_uint, ctypes.POINTER(Statx)]
libc.statx.restype = ctypes.c_int


def mount_id(path, unique):
    mask = 0x4000 if unique else 0x1000  # STATX_MNT_ID_UNIQUE / STATX_MNT_ID
    value = Statx()
    if libc.statx(-100, os.fsencode(path), 0, mask, ctypes.byref(value)):
        number = ctypes.get_errno()
        raise OSError(number, os.strerror(number), path)
    if value.mask & mask != mask or not value.mount_id:
        raise RuntimeError("The cleanup probe requires the requested statx mount identity")
    return value.mount_id
