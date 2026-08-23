import dbus

bus = dbus.SessionBus()
svc = bus.get_object("org.freedesktop.secrets", "/org/freedesktop/secrets")
props = dbus.Interface(svc, "org.freedesktop.DBus.Properties")
print("collections:", list(props.Get("org.freedesktop.Secret.Service", "Collections")))
alias = bus.get_object(
    "org.freedesktop.secrets", "/org/freedesktop/secrets/aliases/default"
)
print(
    "default Locked =",
    alias.Get(
        "org.freedesktop.Secret.Collection",
        "Locked",
        dbus_interface="org.freedesktop.DBus.Properties",
    ),
)
