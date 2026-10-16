#!/bin/sh
# Run on OpenWrt after installing rpcd and uhttpd-mod-ubus.
# A dedicated HTTP listener leaves the existing LuCI/GL.iNet listener intact.
set -eu

: "${DASHBOARD_PASSWORD_HASH:?Set DASHBOARD_PASSWORD_HASH to a crypt hash, e.g. from openssl passwd -6}"
case "$DASHBOARD_PASSWORD_HASH" in
    '$1$'*|'$5$'*|'$6$'*) ;;
    *) echo 'Expected a crypt password hash ($1$, $5$, or $6$).' >&2; exit 1 ;;
esac

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
lan_ip=${DASHBOARD_LAN_IP:-$(uci -q get network.lan.ipaddr)}
lan_ip=${lan_ip%/*}
port=${DASHBOARD_HTTP_PORT:-8080}
case "$lan_ip" in
    ''|0.0.0.0|*[!0-9.]*) echo 'Set DASHBOARD_LAN_IP to the router LAN IPv4 address.' >&2; exit 1 ;;
esac
case "$port" in
    ''|*[!0-9]*) echo 'DASHBOARD_HTTP_PORT must be an integer.' >&2; exit 1 ;;
esac
if [ "$port" -lt 1 ] || [ "$port" -gt 65535 ]; then
    echo 'DASHBOARD_HTTP_PORT must be between 1 and 65535.' >&2
    exit 1
fi
if [ ! -r /usr/lib/uhttpd_ubus.so ]; then
    echo 'Install uhttpd-mod-ubus first.' >&2
    exit 1
fi

mkdir -p /usr/share/rpcd/acl.d /etc/ubus-dashboard-empty
cp "$script_dir/dashboard.json" /usr/share/rpcd/acl.d/ubus-dashboard.json
chmod 644 /usr/share/rpcd/acl.d/ubus-dashboard.json

uci set rpcd.ubus_dashboard=login
uci set rpcd.ubus_dashboard.username=dashboard
uci set "rpcd.ubus_dashboard.password=$DASHBOARD_PASSWORD_HASH"
uci -q delete rpcd.ubus_dashboard.read || true
uci -q delete rpcd.ubus_dashboard.write || true
uci add_list rpcd.ubus_dashboard.read=ubus-dashboard
uci commit rpcd

uci set uhttpd.ubus_dashboard=uhttpd
uci -q delete uhttpd.ubus_dashboard.listen_http || true
uci add_list "uhttpd.ubus_dashboard.listen_http=$lan_ip:$port"
uci set uhttpd.ubus_dashboard.home=/etc/ubus-dashboard-empty
uci set uhttpd.ubus_dashboard.ubus_prefix=/ubus
uci set uhttpd.ubus_dashboard.no_ubusauth=0
uci set uhttpd.ubus_dashboard.rfc1918_filter=1
uci commit uhttpd
/etc/init.d/rpcd restart
/etc/init.d/uhttpd restart
printf 'Dashboard endpoint: http://%s:%s/ubus\n' "$lan_ip" "$port"
