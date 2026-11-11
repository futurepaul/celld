#!/usr/bin/env bash
# One-time install of fragment-celld.service: its system user, binaries,
# directories, settings and unit. Run it as root, after reading it.
#
#   sudo contrib/systemd/install.sh CELLD ESBUILD
#
# It neither starts the service nor touches the sandcastle engine: README.md
# has those steps. Running it again updates the binaries and the unit, keeps
# the settings' contents, and sets every owner and mode again.
set -euo pipefail

user=fragment-celld
die() { echo "install.sh: $*" >&2; exit 1; }
[[ $(id -u) == 0 ]] || die "run it as root"
[[ $# == 2 && -f $1 && -f $2 ]] || { sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)

# A system user: no login shell, no home but its state directory.
if ! id -u "$user" >/dev/null 2>&1; then
  useradd --system --user-group --no-create-home --home-dir "/var/lib/$user" \
    --shell /usr/sbin/nologin --comment "celld for a self-hosted Fragment" "$user"
fi

# The binaries are root's; the service can run them, not change them.
install -d -o root -g root -m 0755 "/usr/local/lib/$user"
install -o root -g root -m 0755 "$1" "/usr/local/lib/$user/celld"
install -o root -g root -m 0755 "$2" "/usr/local/lib/$user/esbuild"

# The cell's project is root's too. Its .celld links to the state directory,
# which systemd creates for the service (StateDirectory=).
install -d -o root -g root -m 0755 /opt/fragment /opt/fragment/cell
state=/opt/fragment/cell/.celld
if [[ -L $state ]]; then
  [[ $(readlink "$state") == "/var/lib/$user/cell" ]] ||
    die "$state links to $(readlink "$state"), not /var/lib/$user/cell; remove it"
elif [[ -e $state ]]; then
  die "$state exists and is not a symlink; move it aside"
else
  ln -s "/var/lib/$user/cell" "$state"
fi

# Settings: root's, readable by the service's group. An existing file keeps
# its contents, not its owner or mode.
install -d -o root -g "$user" -m 0750 "/etc/$user"
env_file=/etc/$user/celld.env
if [[ -L $env_file ]]; then
  die "$env_file is a symlink; make it a file"
elif [[ -e $env_file ]]; then
  chown root:"$user" "$env_file"
  chmod 0640 "$env_file"
else
  install -o root -g "$user" -m 0640 "$here/celld.env" "$env_file"
fi

install -o root -g root -m 0644 "$here/fragment-celld.service" /etc/systemd/system/fragment-celld.service
systemctl daemon-reload
systemd-analyze security --no-pager fragment-celld.service | tail -1
echo "installed: $user is uid $(id -u "$user"), gid $(id -g "$user"); the next steps are in README.md"
