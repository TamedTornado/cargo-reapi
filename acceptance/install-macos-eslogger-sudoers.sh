#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
  echo "this installer is for macOS only" >&2
  exit 2
fi

operator=$(id -un)
case "$operator" in
  *[!A-Za-z0-9._-]*|'')
    echo "unsupported account name: $operator" >&2
    exit 2
    ;;
esac

rule_name=cargo-reapi-eslogger
rule_target=/etc/sudoers.d/$rule_name
rule_staging=$(mktemp "${TMPDIR:-/tmp}/$rule_name.XXXXXX")
trap 'rm -f "$rule_staging"' EXIT HUP INT TERM

printf '%s ALL=(root) NOPASSWD: /usr/bin/eslogger\n' "$operator" >"$rule_staging"
chmod 0440 "$rule_staging"

echo "Validating the scoped sudoers rule..."
/usr/sbin/visudo -cf "$rule_staging"

echo "Installing $rule_target (sudo will prompt once)..."
sudo /usr/bin/install -o root -g wheel -m 0440 "$rule_staging" "$rule_target"

echo "Validating the complete sudoers policy..."
sudo /usr/sbin/visudo -c

if ! sudo -n -l /usr/bin/eslogger >/dev/null 2>&1; then
  echo "installed rule is not active; check that /etc/sudoers includes /etc/sudoers.d" >&2
  exit 1
fi

echo "Ready: /usr/bin/eslogger is passwordless for $operator; all other sudo commands are unchanged."
