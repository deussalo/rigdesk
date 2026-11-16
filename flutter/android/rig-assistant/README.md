# Rig Assistant (dev.rig.assistant)

The phone's assistant inside the RigDesk Android app, as a module of its own: it needs none of RigDesk's screen sharing.

- **Hold Home** opens a sheet that records until **Send** or **Cancel**. When the phone's recogniser stops on silence or a
  timeout it starts again and keeps what was heard. Nothing reaches Rig until Send; Cancel, Back or leaving the sheet drops it.
- **Send** gives what was said (and typed) plus a screenshot to the agent resident on this phone, as one message that wakes it.
  Offline, it is kept and sent when the phone reconnects, once.
- **Tools** for the resident, as `phone/*` through Rig's rig-assistant plugin: `apps`, `open`, `key`, `tell`, `notifications`,
  `notification_reply`, `notification_action`, `notification_dismiss`, `notify`, `sms_send`, `contacts`, `files`, `file_read`,
  `file_write`. `key` needs RigDesk's Input control on; the rest need only their own switch. With RigDesk enabled the same agent
  can hold the phone for `desk/*` too.
- **Link:** one socket the phone dials to `<Rig>/api/contributions/rig-assistant/sockets/phone`, with Rig's access token.

## Setting a phone up

Either open **Rig Assistant** on the phone and fill its setup screen, switch by switch, or from a machine whose adb the
phone trusts run `phone-setup.sh <serial> <Rig's address> <phone name> [apk]`, which does all of it:

1. installs the APK (if given);
2. grants microphone, notifications, SMS and contacts; allows drawing over apps and All files access; exempts the app
   from battery optimisation;
3. turns on notification access for `dev.rig.assistant.RigNotifications`;
4. makes the app the default assistant (`android.app.role.ASSISTANT`, `voice_interaction_service`);
5. writes Rig's address, token and phone name through the `Configure` broadcast, which only adb's shell may send.

Then on Rig, make an agent the phone's resident on the Rig Assistant page (`POST /api/contributions/rig-assistant/routes/resident`).

Each switch hands Rig's agents something the person should know they give: notification access, the text of every
notification (messages, and sign-in codes where Android shows them); SMS, sending from their number at their cost;
contacts, their address book; All files access, everything in the phone's shared storage.
