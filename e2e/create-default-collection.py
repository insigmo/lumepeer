"""Creates the default Secret Service collection headlessly (empty password).

Used by the E2E harness on the Linux VMs: a session reached only over SSH
never ran PAM's keyring unlock, so gnome-keyring has no default collection
and every secret-service call ends in "prompt dismissed". Creating the
default collection with an empty password is what a GUI first-login would
do; afterwards the app's keystore works without any prompt.
"""
import sys
import time

import dbus
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
loop = GLib.MainLoop()

svc = bus.get_object("org.freedesktop.secrets", "/org/freedesktop/secrets")
props = dbus.Interface(svc, "org.freedesktop.DBus.Properties")

result = svc.CreateCollection(
    {"org.freedesktop.Secret.Collection.Label": "Login"},
    "default",
    dbus_interface="org.freedesktop.Secret.Service",
)
collection_path, prompt_path = result
prompt_path = str(prompt_path)
print("collection:", collection_path, "prompt:", prompt_path)

if prompt_path != "/":
    prompt = bus.get_object("org.freedesktop.secrets", prompt_path)

    def on_completed(_prompt, unlocked):
        print("created, unlocked:", list(unlocked))
        loop.quit()

    prompt.connect_to_signal("Completed", on_completed)
    prompt.Prompt("", dbus_interface="org.freedesktop.Secret.Prompt")
    GLib.timeout_add_seconds(15, loop.quit)
    loop.run()

time.sleep(0.5)
cols = list(props.Get("org.freedesktop.Secret.Service", "Collections"))
ok = False
for c in cols:
    o = bus.get_object("org.freedesktop.secrets", c)
    locked = o.Get(
        "org.freedesktop.Secret.Collection",
        "Locked",
        dbus_interface="org.freedesktop.DBus.Properties",
    )
    print(c, "Locked =", bool(locked))
    ok = ok or not locked
sys.exit(0 if ok else 1)
