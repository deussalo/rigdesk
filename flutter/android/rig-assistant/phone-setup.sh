#!/usr/bin/env bash
# Everything the phone itself needs for Rig Assistant, in one go, from a machine whose adb the phone trusts:
#   phone-setup.sh <adb serial> <Rig's https address> <phone name> [apk]
# for example: phone-setup.sh 100.81.214.29:5555 https://vultr.tailfae3b0.ts.net moto-g57 rigdesk.apk
# The access token is read from ~/.config/rig/access-token.env (RIG_ACCESS_TOKEN) and goes to the phone on adb's stdin,
# never on a command line. The phone must be unlocked for the install; nothing here taps the screen.
set -euo pipefail
serial="$1"; url="$2"; phone="$3"; apk="${4:-}"
pkg=com.deussalo.rigdesk
adb="${ADB:-adb} -s $serial"
. "$HOME/.config/rig/access-token.env"
[ -n "$apk" ] && $adb install -r "$apk"
for permission in RECORD_AUDIO POST_NOTIFICATIONS SEND_SMS READ_CONTACTS; do
  $adb shell pm grant $pkg android.permission.$permission
done
$adb shell appops set $pkg SYSTEM_ALERT_WINDOW allow          # tell's bubble over other apps
$adb shell appops set $pkg MANAGE_EXTERNAL_STORAGE allow      # files, file_read, file_write
$adb shell appops set $pkg ACCESS_RESTRICTED_SETTINGS allow || true
$adb shell dumpsys deviceidle whitelist +$pkg                 # the link stays up with the screen off
$adb shell cmd notification disallow_listener $pkg/com.carriez.flutter_hbb.RigNotifications 2>/dev/null || true
$adb shell cmd notification allow_listener $pkg/dev.rig.assistant.RigNotifications
# The default assistant: hold Home opens Rig Assistant's sheet.
for held in $($adb shell cmd role get-role-holders android.app.role.ASSISTANT | tr -d '\r' | tr ';' ' '); do
  [ "$held" = "$pkg" ] || $adb shell cmd role remove-role-holder android.app.role.ASSISTANT "$held" || true
done
$adb shell cmd role add-role-holder android.app.role.ASSISTANT $pkg
$adb shell settings put secure assistant $pkg/dev.rig.assistant.AssistService
$adb shell settings put secure voice_interaction_service $pkg/dev.rig.assistant.AssistService
# Rig's address, the token and the phone's name, by a broadcast only adb's shell may send (it holds WRITE_SECURE_SETTINGS).
printf 'am broadcast -n %s/dev.rig.assistant.Configure --es url %q --es phone %q --es token %q\n' "$pkg" "$url" "$phone" "$RIG_ACCESS_TOKEN" | $adb shell sh
echo "assistant: $($adb shell cmd role get-role-holders android.app.role.ASSISTANT | tr -d '\r')"
echo "voice_interaction_service: $($adb shell settings get secure voice_interaction_service | tr -d '\r')"
