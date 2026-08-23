"""Creates a plain (non-aliased) keyring file by writing an empty-password
keyring in the gnome-keyring on-disk format, then asks the daemon to reload.

Simpler and more robust than driving the CreateCollection prompt: stop the
user's keyring daemon, drop a valid empty `login.keyring` into place, restart
the daemon. The daemon picks the file up as collection 'login' unlocked.
"""
import os
import struct
import subprocess
import sys
import time

HOME = os.path.expanduser("~")
KEYRINGS = os.path.join(HOME, ".local/share/keyrings")
LOGIN = os.path.join(KEYRINGS, "login.keyring")


def empty_keyring() -> bytes:
    # GnomeKeyring store v2 header: magic, major=0 minor=2, flags=1
    # (hashed, but with no items the file stays plaintext), then a name
    # field "Login" of 88 bytes and zero hash/crypt sections. The exact
    # layout below is what gnome-keyring writes for a fresh empty keyring.
    buf = bytearray()
    buf += b"GnomeKeyring\n\r\x00\n\x00\x00\x00\x00"          # magic
    buf += struct.pack(">II", 0, 2)                            # version 0.2
    buf += struct.pack(">I", 1)                                # flags: hashed
    name = b"Login"
    buf += struct.pack(">I", len(name))                        # name length
    buf += name.ljust(88, b"\x00")                             # name field
    buf += b"\x00" * 20                                        # md5 salt slot
    buf += b"\x00" * 20                                        # reserved
    buf += struct.pack(">I", 0)                                # item count
    buf += struct.pack(">I", 0xFFFFFFFF)                       # end sentinel
    return bytes(buf)


def main() -> int:
    os.makedirs(KEYRINGS, exist_ok=True)
    subprocess.run(
        ["pkill", "-u", os.environ.get("USER", "beta"), "-f",
         "gnome-keyring-daemon.*secrets"],
        check=False,
    )
    time.sleep(0.5)
    with open(LOGIN, "wb") as f:
        f.write(empty_keyring())
    os.chmod(LOGIN, 0o600)
    print("wrote", LOGIN)

    # Restart the secrets daemon so it rescans the directory; the session
    # bus address is inherited from this SSH session's user environment.
    env_file = "/run/user/{}/gnome-session-generated-path".format(os.getuid())
    subprocess.Popen(
        ["gnome-keyring-daemon", "--daemonize", "--replace",
         "--components=secrets"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    time.sleep(1.5)
    print("restarted gnome-keyring-daemon")
    return 0


if __name__ == "__main__":
    sys.exit(main())
