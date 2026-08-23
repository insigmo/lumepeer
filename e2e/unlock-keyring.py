import dbus
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib
DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
loop = GLib.MainLoop()
svc = bus.get_object("org.freedesktop.secrets", "/org/freedesktop/secrets")
result = svc.Unlock(["/org/freedesktop/secrets/collection/login"], dbus_interface="org.freedesktop.Secret.Service")
print("raw unlock reply:", result)
unlocked_paths, prompt_path = result
prompt_path = str(prompt_path)
print("prompt:", prompt_path)

done = [False]
def on_completed(prompt, unlocked_paths):
    print("completed, unlocked:", list(unlocked_paths))
    done[0] = True
    loop.quit()

if prompt_path != "/":
    prompt = bus.get_object("org.freedesktop.secrets", prompt_path)
    prompt.connect_to_signal("Completed", on_completed)
    prompt.Prompt("", dbus_interface="org.freedesktop.Secret.Prompt")
    GLib.timeout_add_seconds(10, loop.quit)
    loop.run()
props = bus.get_object("org.freedesktop.secrets", "/org/freedesktop/secrets/collection/login")
print("Locked =", props.Get("org.freedesktop.Secret.Collection", "Locked", dbus_interface="org.freedesktop.DBus.Properties"))
