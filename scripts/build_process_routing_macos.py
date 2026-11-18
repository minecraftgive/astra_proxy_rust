#!/usr/bin/env python3
"""Build the native Network Extension and its activation host. Signing is explicit."""
import argparse
from datetime import datetime, timezone
from fnmatch import fnmatchcase
import platform
import plistlib
import shutil
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "crates/astra-process-routing/native/macos"


def write_plist(path, data):
    with path.open("wb") as file:
        plistlib.dump(data, file)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="new .app directory")
    parser.add_argument("--bundle-id", default="org.astra.ProcessRouting")
    parser.add_argument("--team", help="Apple team ID")
    parser.add_argument("--sign", help="local signing identity")
    parser.add_argument("--host-profile", type=Path)
    parser.add_argument("--extension-profile", type=Path)
    parser.add_argument("--arch", choices=["x86_64", "arm64"], default=platform.machine())
    args = parser.parse_args()
    if args.output.suffix != ".app" or args.output.exists():
        parser.error("output must be a new .app path")
    if any((args.sign, args.team, args.host_profile, args.extension_profile)) and not all(
            (args.sign, args.team, args.host_profile, args.extension_profile)):
        parser.error("signing requires identity, team and both provisioning profiles")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=args.output.parent) as temp:
        root = Path(temp)
        app = root / args.output.name
        ext_id = args.bundle_id + ".extension"
        ext = app / "Contents/Library/SystemExtensions" / (ext_id + ".systemextension")
        group = (args.team or "UNSIGNED") + "." + args.bundle_id
        for bundle, identifier, binary, is_ext in [
            (ext, ext_id, "AstraCapture", True), (app, args.bundle_id, "AstraProcessRouting", False),
        ]:
            contents = bundle / "Contents"
            macos = contents / "MacOS"
            macos.mkdir(parents=True)
            info = dict(CFBundleIdentifier=identifier, CFBundleExecutable=binary, CFBundleName=binary,
                        CFBundlePackageType="SYSX" if is_ext else "APPL", CFBundleVersion="1",
                        CFBundleShortVersionString="0.1.0", LSMinimumSystemVersion="15.0",
                        NSSystemExtensionUsageDescription="Apply the executable routing rules selected in Astra.")
            if is_ext:
                info["NetworkExtension"] = {"NEMachServiceName": group + ".capture",
                    "NEProviderClasses": {"com.apple.networkextension.app-proxy": "AstraProcessProvider"}}
                sources = ["-lbsm", "-import-objc-header", str(SOURCE / "ProcessIdentity.h"),
                           str(SOURCE / "Provider.swift"), str(SOURCE / "Channel.swift"), str(SOURCE / "ExtensionMain.swift")]
            else:
                sources = [str(SOURCE / "Host.swift")]
            write_plist(contents / "Info.plist", info)
            subprocess.run(["xcrun", "swiftc", "-O", "-parse-as-library", "-target",
                            f"{args.arch}-apple-macos15", str(SOURCE / "Policy.swift"), *sources, "-o", str(macos / binary)], check=True)
            if args.sign:
                profile = args.extension_profile if is_ext else args.host_profile
                decoded = plistlib.loads(subprocess.check_output(["security", "cms", "-D", "-i", str(profile)]))
                permitted = decoded["Entitlements"]
                modes = permitted.get("com.apple.developer.networking.networkextension", [])
                mode = next((s for s in ("app-proxy-provider-systemextension", "app-proxy-provider") if s in modes), None)
                if mode is None or args.team not in decoded.get("TeamIdentifier", []):
                    parser.error(f"{identifier} profile has incompatible team or capabilities")
                if decoded["ExpirationDate"].replace(tzinfo=timezone.utc) <= datetime.now(timezone.utc):
                    parser.error(f"{identifier} provisioning profile has expired")
                app_id = args.team + "." + identifier
                allowed_id = permitted.get("com.apple.application-identifier", permitted.get("application-identifier", ""))
                groups = permitted.get("com.apple.security.application-groups", [])
                if not fnmatchcase(app_id, allowed_id) or not any(fnmatchcase(group, allowed) for allowed in groups):
                    parser.error(f"{identifier} profile does not authorize its application identifier or group {group}")
                if not is_ext and not permitted.get("com.apple.developer.system-extension.install"):
                    parser.error(f"{identifier} host profile does not authorize system extension installation")
                entitlements = {"com.apple.developer.networking.networkextension": [mode],
                                "com.apple.developer.team-identifier": args.team,
                                "com.apple.application-identifier": app_id,
                                "com.apple.security.application-groups": [group]}
                if not is_ext:
                    entitlements["com.apple.developer.system-extension.install"] = True
                path = root / (binary + ".entitlements")
                write_plist(path, entitlements)
                shutil.copyfile(profile, contents / "embedded.provisionprofile")
                subprocess.run(["codesign", "--force", "--options", "runtime", "--sign", args.sign,
                                "--entitlements", str(path), str(bundle)], check=True)
                subprocess.run(["codesign", "--verify", "--strict", str(bundle)], check=True)
        app.rename(args.output)


if __name__ == "__main__":
    main()
