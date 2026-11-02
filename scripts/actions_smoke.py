"""Private screenshot storage for Actions browser smoke commands."""
import os
import stat
import tempfile
from pathlib import Path


def private_output_directory(requested, prefix):
    if requested is None:
        return Path(tempfile.mkdtemp(prefix=prefix))
    requested = Path(requested).absolute()
    requested.mkdir(mode=0o700, parents=True, exist_ok=True)
    for directory in (requested, *requested.parents):
        info = directory.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid not in (0, os.getuid()):
            raise ValueError(f"Unsafe screenshot directory: {directory}")
        if info.st_mode & 0o022 and not info.st_mode & stat.S_ISVTX:
            raise ValueError(f"Writable screenshot ancestor: {directory}")
    info = requested.lstat()
    if info.st_uid != os.getuid() or info.st_mode & 0o022:
        raise ValueError(f"Screenshot directory must be owned and writable only by you: {requested}")
    # Never reuse existing screenshot names, even in an explicitly selected directory.
    return Path(tempfile.mkdtemp(prefix=prefix, dir=requested))
