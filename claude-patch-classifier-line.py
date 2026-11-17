#!/usr/bin/env python3
# Point the claude launcher link at a copy of the installed binary with the "Allowed by auto mode classifier" row removed.
# The binary runs the module's embedded bytecode, not its source, so the module graph entry's bytecode offset and
# length are zeroed to make it run the patched source. The installed binary under versions/ is only read.
# Same-length replacement keeps every byte offset.
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile

PATCHED = os.path.expanduser('~/.local/share/claude-patched')
VERSIONS = os.path.expanduser('~/.local/share/claude/versions')
# Minified names change between releases, so match them as identifiers.
ORIG = re.compile(rb'([\w$]+)=[\w$]+&&[\w$]+\.name!==[\w$]+&&e\([\w$]+,\{height:1,children:e\([\w$]+,'
                  rb'\{dimColor:!0,children:"Allowed by auto mode(?: classifier)?"\}\)\}\)')
MARK = b'// @bun @bytecode\n'


def drop_bytecode(data, at):
    start = data.rfind(MARK, 0, at)
    length = data.find(b'\x00', at) - start
    hits = [m.start() for m in re.finditer(re.escape(struct.pack('<I', length)), data[-2**20:])]
    if start < 0 or len(hits) != 1:
        return None
    pos = len(data) - 2**20 + hits[0] + 12
    if struct.unpack('<II', data[pos:pos + 8]) == (0, 0):
        return None
    return data[:pos] + bytes(8) + data[pos + 8:]


def make_copy(src, dst):
    with open(src, 'rb') as f:
        data = f.read()
    found = list(ORIG.finditer(data))
    patched = drop_bytecode(data, found[0].start()) if len(found) == 1 else None
    if patched is None:
        print(f'classifier-line: no unique source row or bytecode entry in {src}, copied unpatched', file=sys.stderr)
    else:
        m = found[0]
        head = m.group(1) + b'=void 0'
        data = patched[:m.start()] + head + b' ' * (m.end() - m.start() - len(head)) + patched[m.end():]
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(dst))
    with os.fdopen(fd, 'wb') as f:
        f.write(data)
    shutil.copymode(src, tmp)
    os.replace(tmp, dst)


def newest_version():
    names = [n for n in os.listdir(VERSIONS) if re.fullmatch(r'\d+(\.\d+)*', n)]
    return os.path.join(VERSIONS, max(names, key=lambda n: tuple(map(int, n.split('.')))))


def main(link):
    src = os.path.realpath(link)
    if src.startswith(PATCHED + os.sep):
        src = newest_version()
    dst = os.path.join(PATCHED, os.path.basename(src), 'claude')
    if not os.path.exists(dst):
        make_copy(src, dst)
        print(f'classifier-line: wrote {dst}', file=sys.stderr)
    if os.path.realpath(link) != dst:
        tmp = link + '.tmp'
        if os.path.lexists(tmp):
            os.remove(tmp)
        os.symlink(dst, tmp)
        os.replace(tmp, link)
    # Keep only the copy the launcher points at; running sessions keep their open binary after trashing.
    for name in os.listdir(PATCHED):
        if name != os.path.basename(src):
            subprocess.run(['gio', 'trash', '--', os.path.join(PATCHED, name)], check=True)
            print(f'classifier-line: trashed {name}', file=sys.stderr)


if __name__ == '__main__':
    main(sys.argv[1])
