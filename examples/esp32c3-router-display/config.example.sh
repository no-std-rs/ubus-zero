# Copy to config.sh, edit, then: . ./config.sh
export WIFI_SSID='your-2.4GHz-network'
export WIFI_PASSWORD='your-wifi-password'
export UBUS_URL='http://192.168.8.1:8080/ubus'
export UBUS_USERNAME='dashboard'
export UBUS_PASSWORD='your-dashboard-password'
# Logical netifd interface (wan, wwan, tethering, ...), NOT its Linux device.
export WAN_INTERFACE='wan'
# Optional AP interface, as listed by: ubus call iwinfo devices
export WIFI_DEVICE=''
