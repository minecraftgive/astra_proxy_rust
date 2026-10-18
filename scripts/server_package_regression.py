#!/usr/bin/env python3
"""Check the independent server archive, its exact file list and clean extracted execution."""
import argparse
import pathlib
import subprocess
import sys
import tarfile
import tempfile
from platform_support import executable
ROOT=pathlib.Path(__file__).resolve().parents[1]

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('archive',type=pathlib.Path);args=parser.parse_args()
    archive=args.archive.resolve()
    with tempfile.TemporaryDirectory(prefix='astra-server-extracted-') as temp:
        directory=pathlib.Path(temp)
        with tarfile.open(archive) as tar:tar.extractall(directory,filter='data')
        roots=list(directory.iterdir());assert len(roots)==1;root=roots[0]
        actual={p.relative_to(root).as_posix() for p in root.rglob('*') if p.is_file()}
        assert root.name=='server'
        assert actual=={executable('astra-server-core'),executable('astra-server-tui'),'server.example.toml'},actual
        assert {p.name for p in root.iterdir()}==actual
        for name in ('astra-server-core','astra-server-tui'):
            subprocess.run([str(root/executable(name)),'--help'],check=True,capture_output=True,timeout=10)
        binary=str(root/executable('astra-server-core'))
        subprocess.run([binary,'--generate-cert',str(root/'tls'),'localhost'],check=True,capture_output=True,timeout=10)
        subprocess.run([binary,'--config',str(root/'server.example.toml'),'--check'],check=True,capture_output=True,timeout=10)
        subprocess.run([sys.executable,str(ROOT/'scripts/server_lifecycle.py'),'--bin-dir',str(root)],check=True,timeout=40)
        print('PASS minimal server archive, executable help, independent extracted lifecycle',flush=True)
if __name__=='__main__':main()
