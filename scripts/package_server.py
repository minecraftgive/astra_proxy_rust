#!/usr/bin/env python3
"""Package the independent server executables and one comprehensive example."""
from package_release import ROOT, arguments, package


def main():
    package('server', ('astra-server-core','astra-server-tui'), ROOT/'configs/server.example.toml', arguments(__doc__))


if __name__ == '__main__':
    main()
