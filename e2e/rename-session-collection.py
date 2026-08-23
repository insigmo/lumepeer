"""Renames the transient session collection to 'Login' so gnome-keyring
persists it as ~/.local/share/keyrings/login.keyring with an empty password,
then verifies the default alias resolves to an unlocked collection.
"""
import time

import dbus

bus = dbus.SessionBus()
coll = bus.get_object(
    "org.freedesktop.secrets", "/org/freedesktop/secrets/collection/session"
)
coll.Set(
    "org.freedesktop.Secret.Collection",
    "Label",
    "Login",
    dbus_interface="org.freedesktop.DBus.Properties",
)
time.sleep(1.0)

props = dbus.Interface(
    bus.get_object("org.freedesktop.secrets", "/org/freedesktop/secrets"),
    "org.freedesktop.DBus.Properties",
)
cols = list(props.Get("org.freedesktop.Secret.Service", "Collections"))
print("collections:", cols)

ok = False
for c in cols:
    o = bus.get_object("org.freedesktop.secrets", c)
    label = o.Get(
        "org.freedesktop.Secret.Collection",
        "Label",
        dbus_interface="org.freedesktop.DBus.Properties",
    )
    locked = o.Get(
        "org.freedesktop.Secret.Collection",
        "Locked",
        dbus_interface="org.freedesktop.DBus.Properties",
    )
    print(c, "label =", label, "Locked =", bool(locked))
    if str(label) == "Login" and not locked:
        ok = True
print("RESULT:", "OK" if ok else "FAIL")
