#!/bin/bash
# RigDesk's health sample on Linux: raw figures as one JSON line, never a verdict (Rig decides). Runs as root from its
# own systemd unit every 10 minutes, read by rigdesk_agent::health::Figures (schema 1), as agent_health.ps1 is on
# Windows. A probe that fails is left out and named in errors; a figure this PC has no source for is simply absent.
# Plain bash and coreutils: a PC need not have jq or python. It reads only system state, never anyone's files.
export LC_ALL=C
ERRORS=()

# A JSON string: backslash and quote escaped, control characters dropped.
js() { local s; s=$(printf '%s' "$1" | tr -d '\000-\037'); s=${s//\\/\\\\}; s=${s//\"/\\\"}; printf '"%s"' "$s"; }
# A JSON number, or null when the value is not one.
jn() { [[ "$1" =~ ^-?[0-9]+(\.[0-9]+)?$ ]] && printf '%s' "$1" || printf null; }
# A JSON array of the arguments, each already JSON.
ja() { local IFS=,; printf '[%s]' "$*"; }
fail() { ERRORS+=("$(js "$1")"); }

# os: the distribution as os-release names it; the build is the running kernel.
OS_CAPTION='' OS_VERSION=''
if [ -r /etc/os-release ]; then
    OS_CAPTION=$(. /etc/os-release && printf '%s' "${PRETTY_NAME:-$NAME}")
    OS_VERSION=$(. /etc/os-release && printf '%s' "${VERSION_ID:-}")
else
    fail "os: /etc/os-release is unreadable"
fi
KERNEL=$(uname -r)
UPTIME=$(cut -d. -f1 /proc/uptime 2>/dev/null) || { fail "uptime: /proc/uptime is unreadable"; UPTIME=0; }

# cpu: its model, and how busy it was over one second.
CPU_NAME=$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | head -1)
cpu_ticks() { awk '/^cpu /{idle=$5+$6; total=0; for(i=2;i<=NF;i++) total+=$i; print total, idle}' /proc/stat; }
read -r T1 I1 < <(cpu_ticks); sleep 1; read -r T2 I2 < <(cpu_ticks)
if [ -n "$T1" ] && [ "$T2" -gt "$T1" ]; then
    CPU_LOAD=$(awk -v t=$((T2 - T1)) -v i=$((I2 - I1)) 'BEGIN{printf "%.1f", 100 * (t - i) / t}')
else
    CPU_LOAD=null; fail "cpu: /proc/stat gave no load"
fi

# ram: what the kernel says is available to start new work, not merely unused.
RAM_TOTAL=$(awk '/^MemTotal:/{print int($2/1024)}' /proc/meminfo)
RAM_FREE=$(awk '/^MemAvailable:/{print int($2/1024)}' /proc/meminfo)
[ -n "$RAM_TOTAL" ] || { fail "ram: /proc/meminfo is unreadable"; RAM_TOTAL=0; RAM_FREE=0; }

# volumes: each local block device's file system once, by its mount point.
VOLUMES=()
declare -A SEEN=()
# df exits non-zero when any one file system is unreadable, and still lists the rest.
DF=$(df -l -P -BM -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs -x ramfs 2>&1); DF_EXIT=$?
[ "$DF_EXIT" = 0 ] || fail "volumes: df exited $DF_EXIT: $(printf '%s\n' "$DF" | grep -v '^Filesystem' | grep -v '^/' | head -1)"
while read -r dev total avail target; do
    [[ "$dev" == /dev/* ]] || continue
    [ -n "${SEEN[$dev]:-}" ] && continue
    SEEN[$dev]=1
    label=$(lsblk -no LABEL "$dev" 2>/dev/null | head -1)
    VOLUMES+=("{\"name\":$(js "$target"),\"label\":$(js "$label"),\"total_mb\":$(jn "${total%M}"),\"free_mb\":$(jn "${avail%M}")}")
done < <(printf '%s\n' "$DF" | awk 'NR>1 && $1 ~ /^\// {print $1, $2, $4, $6}')

# pending reboot: Debian's and Ubuntu's marker, and a kernel newer than the running one.
REBOOT_WHY=()
[ -e /run/reboot-required ] && REBOOT_WHY+=("$(js "packages: $(tr '\n' ' ' < /run/reboot-required.pkgs 2>/dev/null | sed 's/ *$//')")")
NEWEST_KERNEL=$(ls /boot/vmlinuz-* 2>/dev/null | sed 's|^/boot/vmlinuz-||' | sort -V | tail -1)
[ -n "$NEWEST_KERNEL" ] && [ "$NEWEST_KERNEL" != "$KERNEL" ] && REBOOT_WHY+=("$(js "kernel $NEWEST_KERNEL installed, $KERNEL running")")
[ ${#REBOOT_WHY[@]} -gt 0 ] && PENDING_REBOOT=true || PENDING_REBOOT=false

# updates: from apt's cache as it stands; this never fetches, so checked_ms says how fresh the cache is.
UPDATES=null
if command -v apt-get > /dev/null; then
    # A broken cache or a held lock fails the simulation: then pending is unknown, not 0.
    APT_ERROR=null
    if SIMULATED=$(apt-get -s -o Debug::NoLocking=1 upgrade 2>&1); then
        PENDING=$(printf '%s\n' "$SIMULATED" | grep -c '^Inst ')
    else
        PENDING=null; APT_ERROR=$(js "apt-get -s upgrade failed: $(printf '%s\n' "$SIMULATED" | grep -E '^(E|W):' | head -1)")
    fi
    CHECKED=$(stat -c %Y /var/lib/apt/periodic/update-success-stamp /var/lib/apt/lists/partial 2>/dev/null | sort -n | tail -1)
    LAST=$(zgrep -h ' status installed ' /var/log/dpkg.log* 2>/dev/null | sort | tail -1 | cut -d' ' -f1,2)
    LAST_MS=null; [ -n "$LAST" ] && LAST_MS=$(date -d "$LAST" +%s%3N 2>/dev/null || echo null)
    UPDATES="{\"checked_ms\":$( [ -n "$CHECKED" ] && echo $((CHECKED * 1000)) || echo null),\"last_install_ms\":$(jn "$LAST_MS"),\"pending\":$(jn "$PENDING"),\"error\":$APT_ERROR}"
fi

# disks: each physical disk, with SMART's verdict, wear and temperature where smartctl is installed.
DISKS=()
while read -r name rota tran; do
    media=$([ "$tran" = nvme ] && echo NVMe || { [ "$rota" = 1 ] && echo HDD || echo SSD; })
    health=Unknown wear=null temp=null
    if command -v smartctl > /dev/null; then
        # -n standby leaves a spun-down disk asleep (it reads Unknown that time) rather than waking it every 10 minutes.
        smart=$(smartctl -n standby -H -A "/dev/$name" 2>/dev/null)
        case "$smart" in
            *"test result: PASSED"*|*"SMART Health Status: OK"*) health=Healthy ;;
            *"test result: FAILED"*) health=Unhealthy ;;
        esac
        wear=$(printf '%s\n' "$smart" | sed -n 's/^Percentage Used:[[:space:]]*\([0-9]*\)%.*/\1/p' | head -1)
        temp=$(printf '%s\n' "$smart" | sed -n 's/^Temperature:[[:space:]]*\([0-9]*\) Celsius.*/\1/p' | head -1)
        [ -n "$temp" ] || temp=$(printf '%s\n' "$smart" | awk '$2 ~ /^(Temperature_Celsius|Airflow_Temperature_Cel)$/ {print $10; exit}')
    fi
    DISKS+=("{\"name\":$(js "/dev/$name"),\"media\":$(js "$media"),\"health\":$(js "$health"),\"wear_pct\":$(jn "${wear:-null}"),\"temperature_c\":$(jn "${temp:-null}")}")
done < <(lsblk -dn -o NAME,TYPE,ROTA,TRAN 2>/dev/null | awk '$2 == "disk" && $1 !~ /^(loop|zram|ram)/ {print $1, $3, $4}')

# stopped services: units that failed, with the code their main process last exited with. A transient unit (a login
# session's scope, a systemd-run command) and a user's own manager (user@UID) are none of the services the PC runs: a
# login that ended badly leaves one failed until the next boot.
STOPPED=()
FAILED_UNITS=$(systemctl list-units --state=failed --no-legend --plain 2>&1) || { fail "services: systemctl list-units failed: $(printf '%s' "$FAILED_UNITS" | head -1)"; FAILED_UNITS=''; }
while read -r unit; do
    [ -n "$unit" ] || continue
    case "$unit" in *.scope|user@*.service) continue;; esac
    [ "$(systemctl show -p Transient --value "$unit" 2>/dev/null)" = yes ] && continue
    desc=$(systemctl show -p Description --value "$unit" 2>/dev/null)
    code=$(systemctl show -p ExecMainStatus --value "$unit" 2>/dev/null)
    # A unit can fail with a clean exit (a time-out, a failed condition): it failed all the same.
    [ "${code:-0}" = 0 ] && code=-1
    STOPPED+=("{\"name\":$(js "$unit"),\"display\":$(js "$desc"),\"delayed\":false,\"exit_code\":$(jn "$code")}")
done < <(printf '%s\n' "$FAILED_UNITS" | awk '{print $1}')

# system log: errors and worse in the system journal over the last 24 hours.
if JOURNAL=$(journalctl --system -q --no-pager -p err --since "-24h" -o cat 2>/dev/null); then
    SYSTEM_ERRORS=0; [ -n "$JOURNAL" ] && SYSTEM_ERRORS=$(printf '%s\n' "$JOURNAL" | wc -l)
else
    fail "system log: journalctl failed"; SYSTEM_ERRORS=null
fi
unset JOURNAL

# battery: the first, where there is one.
BATTERY=null
for bat in /sys/class/power_supply/BAT*; do
    [ -r "$bat/capacity" ] || continue
    BATTERY="{\"charge_pct\":$(jn "$(cat "$bat/capacity")"),\"status\":$(js "$(tr 'A-Z' 'a-z' < "$bat/status" 2>/dev/null)")}"
    break
done

# This script's own cost: its shell and every probe it waited for.
read -r CPU_U CPU_S CPU_CU CPU_CS < <(awk '{print $14, $15, $16, $17}' /proc/$$/stat)
CPU_MS=$(( (CPU_U + CPU_S + CPU_CU + CPU_CS) * 1000 / $(getconf CLK_TCK) ))

printf '{"schema":1,"os":{"caption":%s,"version":%s,"build":%s},"uptime_s":%s,"cpu":{"name":%s,"load_pct":%s},"ram":{"total_mb":%s,"free_mb":%s},"volumes":%s,"pending_reboot":%s,"pending_reboot_why":%s,"updates":%s,"defender":null,"bitlocker":[],"disks":%s,"stopped_services":%s,"system_errors_24h":%s,"battery":%s,"cpu_ms":%s,"errors":%s}\n' \
    "$(js "$OS_CAPTION")" "$(js "$OS_VERSION")" "$(js "$KERNEL")" "$(jn "$UPTIME")" "$(js "$CPU_NAME")" "$CPU_LOAD" "$RAM_TOTAL" "$RAM_FREE" \
    "$(ja "${VOLUMES[@]}")" "$PENDING_REBOOT" "$(ja "${REBOOT_WHY[@]}")" "$UPDATES" "$(ja "${DISKS[@]}")" "$(ja "${STOPPED[@]}")" \
    "$(jn "$SYSTEM_ERRORS")" "$BATTERY" "$(jn "$CPU_MS")" "$(ja "${ERRORS[@]}")"
