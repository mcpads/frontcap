#!/system/bin/sh
export FC_MANUAL=1
export FC_HOST_RENDER=1
export LD_PRELOAD="$(dirname "$0")/libvclock.so"
exec "$@"
