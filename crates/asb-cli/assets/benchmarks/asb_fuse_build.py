"""Build the staged libfuse source against the current guest environment."""
from pathlib import Path
import shutil
import subprocess


def build_libfuse(source, destination):
    """Return the built library; retain build logs and partial files on failure."""
    source, destination = Path(source).resolve(), Path(destination).resolve()
    if not source.is_file():
        raise RuntimeError('staged libfuse source is missing; reinstall fuse-overlayfs')
    missing = [name for name in ('tar', 'make', 'cc') if shutil.which(name) is None]
    if missing:
        raise RuntimeError('libfuse build needs guest tools: ' + ', '.join(missing))
    destination.mkdir(mode=0o700, parents=True, exist_ok=False)
    log = destination / 'build.log'
    commands = [
        (['tar', '-xzf', str(source), '--strip-components=1', '-C', str(destination)], destination),
        ([str(destination / 'configure'), '--disable-example', '--disable-util', '--disable-mtab'], destination),
        (['make', '-C', 'lib', '-j2'], destination),
    ]
    with log.open('wb') as output:
        for command, cwd in commands:
            try:
                result = subprocess.run(command, cwd=cwd, stdin=subprocess.DEVNULL,
                                        stdout=output, stderr=subprocess.STDOUT, check=False)
            except OSError as error:
                raise RuntimeError(f'cannot start libfuse build; inspect {log}') from error
            if result.returncode:
                raise RuntimeError(f'libfuse build failed; inspect {log}')
    library = destination / 'lib/.libs/libfuse.so.2.9.9'
    if not library.is_file():
        raise RuntimeError(f'libfuse build did not produce its library; inspect {log}')
    return library


if __name__ == '__main__':
    import sys
    if len(sys.argv) != 3:
        raise SystemExit('usage: asb_fuse_build.py SOURCE_ARCHIVE DESTINATION')
    print(build_libfuse(sys.argv[1], sys.argv[2]))
