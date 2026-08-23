#!/bin/bash
# Unlock the gnome-keyring 'login' collection headlessly with the empty
# password (typical for VMs where PAM never ran the unlock).
export DISPLAY="${DISPLAY:-:0}"
R=$(gdbus call --session --dest org.freedesktop.secrets \
  --object-path /org/freedesktop/secrets \
  --method org.freedesktop.Secret.Service.Unlock "['/org/freedesktop/secrets/collection/login']")
echo "unlock call: $R"
P=$(echo "$R" | grep -oE "prompt/[A-Za-z0-9]+" | head -1)
if [ -n "$P" ]; then
  echo "running prompt /org/freedesktop/secrets/$P"
  timeout 10 gdbus call --session --dest org.freedesktop.secrets \
    --object-path "/org/freedesktop/secrets/$P" \
    --method org.freedesktop.Secret.Prompt.WindowId "" || true
  sleep 1
fi
gdbus call --session --dest org.freedesktop.secrets \
  --object-path /org/freedesktop/secrets/collection/login \
  --method org.freedesktop.DBus.Properties.Get org.freedesktop.Secret.Collection Locked
