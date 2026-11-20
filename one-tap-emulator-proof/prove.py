#!/usr/bin/env python3
"""Throwaway: RigDesk slice D (one tap) on an emulator, with the real APK (RigDesk with its Rig Assistant module) and the real
rigdesk-bridge. Rig itself is stood in for at its two edges, and that is to be said with any result: a stdlib WebSocket server
speaks rig-assistant's phone socket (hello, ack, join_request, join_end, join frames, as phones.py defines them), and this script
sends the bridge the /offer the rigdesk plugin sends once the phone says ready. The plugin's side of both is proved by its tests.
Arranged as in the stale-dialog proof: 100.64.0.2 on both ends, Rig's pairing key planted in the device's local config, taps by adb."""
import base64, hashlib, json, os, re, socket, struct, subprocess, sys, threading, time, urllib.request
from pathlib import Path

PKG = "com.deussalo.rigdesk"
SERVICE = f"{PKG}/com.carriez.flutter_hbb.MainService"
ADDR, HOST_PORT, FWD_PORT, DEV_PORT, DIRECT = "100.64.0.2", 21218, 21318, 21418, 21118
API_PORT, STUB_PORT = 21120, 21500
API_TOKEN, STUB_TOKEN = "proof-bridge-token-0123456789", "proof-rig-token-0123456789"
PASSWORD = "Proof-Pass-7391"
AGENT = {"id": "agent_proof", "title": "Proof Agent", "graph": "proof"}
TITLE = "Proof Agent asks to use this phone"
tools, apk, out = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
out.mkdir(parents=True, exist_ok=True)
BRIDGE, KEY = tools / "rigdesk-bridge", out / "pair.key"
results = []
BUILD_HEAD = os.environ.get("BUILD_HEAD", "unknown")
ARRANGED = ("Rig stood in for at its edges (rig-assistant's phone socket and the plugin's /offer); 100.64.0.2 on both ends; pairing key planted; "
            "taps by adb on the screens a person sees, Start now included (no appops for capture)")

def log(*a): print(time.strftime("%H:%M:%S"), *a, flush=True)
def run(*c, check=True, timeout=60, input=None):
    r = subprocess.run(c, capture_output=True, text=True, timeout=timeout, input=input)
    if check and r.returncode: raise SystemExit(f"{c} -> {r.returncode}\n{r.stdout}\n{r.stderr}")
    return r.stdout
def adb(*c, **k): return run("adb", *c, **k)
def sh(cmd, **k): return adb("shell", cmd, **k)
def record(name, ok, detail):
    results.append({"round": name, "ok": bool(ok), "head": BUILD_HEAD, "arranged": ARRANGED, "detail": detail})
    log("PASS" if ok else "FAIL", name, detail)
    (out / "results.json").write_text(json.dumps(results, indent=1))
def until(fn, seconds, every=0.25):
    end = time.time() + seconds
    while time.time() < end:
        v = fn()
        if v: return v
        time.sleep(every)
    return fn()

# --- Rig's phone socket, stood in for ---------------------------------------------------------------------------------
class RigStub:
    """rig-assistant's /sockets/phone: takes the phone's hello, acks every seq'd frame, keeps what the phone said."""
    def __init__(self):
        self.frames, self.conn, self.lock = [], None, threading.Lock()
        l = socket.socket(); l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); l.bind(("127.0.0.1", STUB_PORT)); l.listen(4)
        threading.Thread(target=self.accept, args=(l,), daemon=True).start()
        threading.Thread(target=self.ping, daemon=True).start()
    def accept(self, l):
        while True:
            c, _ = l.accept(); threading.Thread(target=self.serve, args=(c,), daemon=True).start()
    def serve(self, c):
        try:
            f = c.makefile("rb"); head = b""
            while not head.endswith(b"\r\n\r\n"): head += f.read(1) or b"\r\n\r\n"
            lines = head.decode(errors="replace").split("\r\n"); hdr = {k.strip().lower(): v.strip() for k, _, v in (x.partition(":") for x in lines[1:] if x)}
            if hdr.get("authorization") != f"Bearer {STUB_TOKEN}" or "/api/contributions/rig-assistant/sockets/phone" not in lines[0]:
                log("stub: refused", lines[0], hdr.get("authorization", "")[:12]); c.sendall(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"); c.close(); return
            accept = base64.b64encode(hashlib.sha1((hdr["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            c.sendall(f"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n".encode())
            with self.lock: self.conn = c
            while True:
                op, payload = self.read(f)
                if op == 0x8: break
                if op == 0x9: self.write(c, 0xA, payload); continue
                if op != 0x1: continue
                frame = json.loads(payload); self.frames.append((time.time(), frame)); log("stub: phone said", {k: v for k, v in frame.items() if k != "tools"})
                if frame.get("type") == "hello": self.send({"type": "welcome", "resident": AGENT["title"]})
                if "seq" in frame: self.send({"type": "ack", "seq": frame["seq"]})
        except Exception as e: log("stub: connection ended:", type(e).__name__, e)
        finally:
            with self.lock:
                if self.conn is c: self.conn = None
            try: c.close()
            except OSError: pass
    @staticmethod
    def read(f):
        b1, b2 = f.read(2); n = b2 & 0x7f
        if n == 126: n = struct.unpack(">H", f.read(2))[0]
        elif n == 127: n = struct.unpack(">Q", f.read(8))[0]
        mask = f.read(4) if b2 & 0x80 else b"\0\0\0\0"
        data = bytes(x ^ mask[i % 4] for i, x in enumerate(f.read(n)))
        return b1 & 0x0f, data
    @staticmethod
    def write(c, op, data):
        n = len(data); head = bytes([0x80 | op]) + (bytes([n]) if n < 126 else bytes([126]) + struct.pack(">H", n) if n < 65536 else bytes([127]) + struct.pack(">Q", n))
        c.sendall(head + data)
    def send(self, frame):
        with self.lock:
            if self.conn is None: raise RuntimeError("the phone is not connected to the stub")
            self.write(self.conn, 0x1, json.dumps(frame).encode())
    def ping(self):
        while True:
            time.sleep(20)
            with self.lock:
                try:
                    if self.conn is not None: self.write(self.conn, 0x9, b"")
                except OSError: pass
    def connected(self): return self.conn is not None
    def hello_since(self, t0): return any(t >= t0 and f.get("type") == "hello" for t, f in self.frames)
    def said(self, ask, since):
        return next((f for t, f in self.frames if t >= since and f.get("type") == "join" and f.get("ask") == ask), None)

# --- device and network, as the stale-dialog proof set them up --------------------------------------------------------
def data_dir(): return sh(f"pm path {PKG} >/dev/null; echo /data/data/{PKG}").strip()
def put(path, text):
    tmp = out / "put.tmp"; tmp.write_text(text); adb("push", str(tmp), "/data/local/tmp/put.tmp")
    d = data_dir(); owner = sh(f"stat -c %u:%g {d}").strip()
    sh(f"mkdir -p $(dirname {path}) && cp /data/local/tmp/put.tmp {path} && chown {owner} {path} && chmod 660 {path} && restorecon {path}")

def set_up_device(public):
    adb("root"); time.sleep(3); adb("wait-for-device")
    adb("install", "-r", "-g", str(apk), timeout=240)
    sh(f"pm grant {PKG} android.permission.POST_NOTIFICATIONS", check=False)
    sh(f"am start -W -n {PKG}/com.carriez.flutter_hbb.MainActivity", timeout=90)
    d = data_dir(); prefs = f"{d}/shared_prefs/KEY_SHARED_PREFERENCES.xml"
    got = until(lambda: "KEY_APP_DIR_CONFIG_PATH" in sh(f"cat {prefs} 2>/dev/null", check=False), 90, 2)
    if not got: raise SystemExit("the app never recorded its config path:\n" + sh(f"ls -R {d}/shared_prefs", check=False))
    conf = re.search(r'KEY_APP_DIR_CONFIG_PATH">([^<]+)<', sh(f"cat {prefs}")).group(1)
    until(lambda: sh(f"ls {conf}/*.toml 2>/dev/null", check=False).strip(), 30, 1)
    sh(f"am force-stop {PKG}"); time.sleep(2)
    files = sh(f"ls {conf}/*.toml").split(); log("config files", files)
    second = next(f for f in files if f.endswith("2.toml"))
    main = second[:-len("2.toml")] + ".toml"
    text = "".join(l + "\n" for l in sh(f"cat {main} 2>/dev/null", check=False).splitlines() if not l.startswith("password"))
    put(main, f"password = '{PASSWORD}'\n" + text)
    put(second, "".join(l + "\n" for l in sh(f"cat {second}").splitlines() if "stop-service" not in l))
    local = second[:-len("2.toml")] + "_local.toml"
    lines = [l for l in sh(f"cat {local} 2>/dev/null", check=False).splitlines() if not l.startswith("rigdesk-invite-key")]
    planted = f"rigdesk-invite-key = '{public}'"
    lines = [x for l in lines for x in ([l, planted] if l.strip() == "[options]" else [l])]
    put(local, "\n".join(lines if planted in lines else lines + ["[options]", planted]) + "\n")
    # Overlay and files only, as a phone set up earlier has them: capture is NOT allowed by appops, so Start now shows.
    for op in ("SYSTEM_ALERT_WINDOW", "MANAGE_EXTERNAL_STORAGE"): sh(f"appops set {PKG} {op} allow", check=False)

def pump(a, b):
    try:
        while (d := a.recv(65536)): b.sendall(d)
    except OSError: pass
    for s in (a, b):
        try: s.shutdown(socket.SHUT_RDWR)
        except OSError: pass
def host_relay():
    l = socket.socket(); l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); l.bind((ADDR, HOST_PORT)); l.listen(16)
    def serve():
        while True:
            c, _ = l.accept(); s = socket.create_connection(("127.0.0.1", FWD_PORT))
            for x, y in ((c, s), (s, c)): threading.Thread(target=pump, args=(x, y), daemon=True).start()
    threading.Thread(target=serve, daemon=True).start()
def set_up_network(relay):
    run("sudo", "ip", "addr", "add", f"{ADDR}/32", "dev", "lo")
    sh(f"ip addr add {ADDR}/32 dev lo")
    adb("push", str(relay), "/data/local/tmp/relay"); sh("chmod 755 /data/local/tmp/relay")
    subprocess.Popen(["adb", "shell", f"/data/local/tmp/relay {DEV_PORT} {DIRECT}"], stdout=open(out / "relay.log", "w"), stderr=subprocess.STDOUT)
    adb("forward", f"tcp:{FWD_PORT}", f"tcp:{DEV_PORT}")
    adb("reverse", f"tcp:{STUB_PORT}", f"tcp:{STUB_PORT}")
    host_relay()

def api(method, path, body=None):
    r = urllib.request.Request(f"http://127.0.0.1:{API_PORT}{path}", method=method, data=None if body is None else json.dumps(body).encode(),
                               headers={"Authorization": f"Bearer {API_TOKEN}", "Content-Type": "application/json"})
    with urllib.request.urlopen(r, timeout=10) as a: return json.load(a)

class Events:
    """The bridge's /events stream, as the plugin hears it."""
    def __init__(self):
        self.seen = []
        threading.Thread(target=self.run, daemon=True).start()
    def run(self):
        r = urllib.request.Request(f"http://127.0.0.1:{API_PORT}/events", headers={"Authorization": f"Bearer {API_TOKEN}", "Accept": "text/event-stream"})
        while True:
            try:
                with urllib.request.urlopen(r, timeout=600) as stream:
                    for line in stream:
                        line = line.decode(errors="replace").strip()
                        if line.startswith("data:"):
                            try: self.seen.append((time.time(), json.loads(line[5:])))
                            except ValueError: pass
            except Exception as e:
                log("events:", e); time.sleep(1)
    def answer(self, ask, seconds):
        def got(): return next((e.get("answer") for _, e in self.seen if e.get("type") == "offer" and e.get("ask") == ask), None)
        return until(got, seconds, 0.2)

# --- what a person sees ------------------------------------------------------------------------------------------------
def shot(name):
    with open(out / f"{name}.png", "wb") as f: subprocess.run(["adb", "exec-out", "screencap", "-p"], stdout=f, timeout=30)
def dump():
    sh("rm -f /data/local/tmp/ui.xml", check=False)
    try: sh("uiautomator dump /data/local/tmp/ui.xml 2>&1", check=False, timeout=20)
    except subprocess.TimeoutExpired: return ""
    xml = sh("cat /data/local/tmp/ui.xml 2>/dev/null", check=False)
    return xml if "<hierarchy" in xml else ""
def nodes(xml):
    return [(t, d, int(a), int(b), int(c), int(e)) for t, d, a, b, c, e in
            re.findall(r'<node [^>]*?text="([^"]*)"[^>]*?content-desc="([^"]*)"[^>]*?bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"', xml)]
def find(xml, *labels):
    """The centre of the last node labelled so: a dialog comes after the page it covers, so this is the one on top."""
    want = [l.lower() for l in labels]
    hits = [n for n in nodes(xml) if (n[0].strip().lower() in want or n[1].strip().lower() in want)]
    return ((hits[-1][2] + hits[-1][4]) // 2, (hits[-1][3] + hits[-1][5]) // 2) if hits else (None, None)
def tap_label(*labels, seconds=20, name=None):
    """Taps the first on-screen node whose text is one of labels; shots the screen first under name. The tapped label, or None."""
    end, xml = time.time() + seconds, ""
    while time.time() < end:
        xml = dump()
        x, y = find(xml, *labels)
        if x is not None:
            if name: shot(name); (out / f"{name}.xml").write_text(xml)
            said = next(n[0] or n[1] for n in nodes(xml) if (n[0].strip().lower() in [l.lower() for l in labels] or n[1].strip().lower() in [l.lower() for l in labels]))
            sh(f"input tap {x} {y}"); return said
        time.sleep(1)
    if name: shot(f"{name}-missing"); (out / f"{name}-missing.xml").write_text(xml)
    return None

def posted():
    """Rig Assistant's join notifications as the notification service holds them: their titles."""
    dump_ = sh("dumpsys notification --noredact", timeout=30)
    return [m.group(1) for b in re.split(r"(?=NotificationRecord\()", dump_) if f"pkg={PKG}" in b
            for m in [re.search(r"android\.title=String \((.* asks to use this phone)\)", b)] if m]
def rig_accepted_posted():
    """The notification the phone falls back to when the operator's Accept could not raise Start now: its titles."""
    dump_ = sh("dumpsys notification --noredact", timeout=30)
    return [m.group(1) for b in re.split(r"(?=NotificationRecord\()", dump_) if f"pkg={PKG}" in b
            for m in [re.search(r"android\.title=String \((.*: Arian accepted in Rig)\)", b)] if m]
def overlay(allowed):
    sh(f"appops set {PKG} SYSTEM_ALERT_WINDOW {'allow' if allowed else 'deny'}", check=False)
def login_prompts():
    """RigDesk's ordinary request-to-join notifications (the second Allow one tap must not show)."""
    dump_ = sh("dumpsys notification --noredact", timeout=30)
    return [m.group(1) for b in re.split(r"(?=NotificationRecord\()", dump_) if f"pkg={PKG}" in b
            for m in [re.search(r"android\.title=String \((.*) is trying to join this device\)", b)] if m]
def joined_notices():
    """The phone's honest notices for a session that is in, as "<title>|<text>": "<agent> joined|By <how>. ..."."""
    dump_ = sh("dumpsys notification --noredact", timeout=30)
    return [f"{m.group(1)}|{x.group(1) if x else ''}" for b in re.split(r"(?=NotificationRecord\()", dump_) if f"pkg={PKG}" in b
            for m in [re.search(r"android\.title=String \((.* joined)\)", b)] if m
            for x in [re.search(r"android\.text=String \((.*?)\)\n", b)]]
def autogrouped():
    """RigDesk notifications Android folded under its own summary (group key "ranker_group"), whose public face is the bare app name."""
    dump_ = sh("dumpsys notification --noredact", timeout=30)
    return [b.split("\n", 1)[0] for b in re.split(r"(?=NotificationRecord\()", dump_) if f"pkg={PKG}" in b and "ranker_group" in b]
def notice_drawn(xml, title, how):
    """The collapsed row shows both: the title, and the text's start that says how; and the invite's row is drawn with its
    title, not as the bare app name its missing public version left while the screen is shared; and nothing says RustDesk."""
    return title in xml and f"By {how}." in xml and "Invite a Rig agent" in xml and "RustDesk" not in xml
def shade_shot(name, want=""):
    """The shade as a person pulls it down, waiting up to 10 s for want to be drawn in it."""
    sh("cmd statusbar expand-notifications", check=False); time.sleep(2)
    xml = until(lambda: (lambda x: x if want in x else "")(dump()), 10, 1) or dump()
    shot(name); (out / f"{name}.xml").write_text(xml)
    records = [b for b in re.split(r"(?=NotificationRecord\()", sh("dumpsys notification --noredact", check=False, timeout=30)) if f"pkg={PKG}" in b]
    (out / f"{name}-records.txt").write_text("\n\n".join(b[:3000] for b in records))
    if want and want not in xml:
        # Not drawn: open the app's rows that show only their public face, and look again.
        for _, d, a, b, c, e in [n for n in nodes(xml) if n[0] == "RigDesk"]:
            sh(f"input tap {(a + c) // 2} {(b + e) // 2}"); time.sleep(2)
        xml2 = dump(); shot(f"{name}-opened"); (out / f"{name}-opened.xml").write_text(xml2)
    collapse()
    return xml
def share_screen_page():
    """RigDesk opened on its Share screen page, where its connection cards and requests show."""
    sh(f"am start -W -n {PKG}/com.carriez.flutter_hbb.MainActivity", check=False, timeout=60); time.sleep(3)
    # Flutter's bottom bar names each tab in its content-desc as "<label>\nTab n of 4".
    for _ in range(10):
        hit = next((((a + c) // 2, (b + e) // 2) for _, d, a, b, c, e in nodes(dump()) if d.split("&#10;")[0].split("\n")[0].strip() == "Share screen"), None)
        if hit: sh(f"input tap {hit[0]} {hit[1]}"); break
        time.sleep(1)
    time.sleep(2)
def app_shot(name):
    share_screen_page()
    xml = dump(); shot(name); (out / f"{name}.xml").write_text(xml)
    return xml
NAMED = f"{AGENT['title']} (Rig agent)"
def notification_on_screen(name):
    """Opens the shade and expands our notification so its Accept and Deny show; shots it. True if both show."""
    sh("cmd statusbar expand-notifications", check=False); time.sleep(1.5)
    xml = ""
    for attempt in range(8):
        xml = dump(); ns = nodes(xml)
        labels = {x.strip().lower() for n in ns for x in n[:2]}
        if "accept" in labels and "deny" in labels: break
        ours = [n for n in ns if "asks to use this phone" in (n[0] + n[1])]
        closed = [n for n in ns if n[1].strip().lower() == "expand"]
        if ours: _, _, a, b, c, e = ours[0]; sh(f"input swipe {(a + c) // 2} {b + 5} {(a + c) // 2} {e + 300} 300")
        elif closed: _, _, a, b, c, e = closed[0]; sh(f"input tap {(a + c) // 2} {(b + e) // 2}")
        time.sleep(1)
    shot(name); (out / f"{name}.xml").write_text(xml)
    labels = {x.strip().lower() for n in nodes(xml) for x in n[:2]}
    return "accept" in labels and "deny" in labels
def collapse(): sh("cmd statusbar collapse", check=False); time.sleep(1)
def service_running(): return "MainService" in sh(f"dumpsys activity services {PKG}", check=False, timeout=20)
def listening(): return f":{DIRECT:04X}" in sh("cat /proc/net/tcp /proc/net/tcp6", check=False)

def main():
    relay = Path(sys.argv[4])
    public = run(str(BRIDGE), "--pair-key", str(KEY)).strip(); log("bridge public key", public)
    set_up_device(public); set_up_network(relay)
    stub = RigStub()
    token_file = out / "bridge.token"; token_file.write_text(API_TOKEN)
    env = dict(os.environ, RIGDESK_PAIR_KEY_FILE=str(KEY), RUST_LOG="info")
    password_file = out / "phone.password"; password_file.write_text(PASSWORD)
    def start_bridge(password=False):
        saved = ["--password-file", str(password_file)] if password else []
        return subprocess.Popen([str(BRIDGE), "--peer", f"{ADDR}:{HOST_PORT}", "--token-file", str(token_file), "--port", str(API_PORT), "--journal", str(out / "journal.jsonl")] + saved,
                                env=env, stdout=open(out / "bridge.log", "a"), stderr=subprocess.STDOUT)
    def health():
        try: return api("GET", "/health")
        except Exception: return None
    state = {"bridge": start_bridge()}
    if not until(health, 30, 1): raise SystemExit("the bridge's API never answered")
    events = Events()
    counter = iter(range(1, 1000))

    def fresh_phone():
        """RigDesk stopped (no service, nothing listening) and its Rig Assistant link up to the stub, as a phone in a pocket."""
        collapse(); sh(f"am force-stop {PKG}"); time.sleep(2); t0 = time.time()
        said = sh(f"am broadcast -n {PKG}/dev.rig.assistant.Configure --es url http://127.0.0.1:{STUB_PORT} --es token {STUB_TOKEN} --es phone moto", timeout=30)
        if not until(lambda: stub.hello_since(t0) and stub.connected(), 40, 0.5): raise RuntimeError(f"the phone never said hello to the stub: {said.strip()}")
        sh("input keyevent KEYCODE_HOME", check=False); time.sleep(1)
        return {"service_running": service_running(), "listening": listening()}
    def request(lapse_s=180):
        n = next(counter); ask, nonce = f"desk_tap{n:03d}", f"nonce-{n:03d}-{os.urandom(8).hex()}"
        t0 = time.time()
        stub.send({"type": "join_request", "ask": ask, "nonce": nonce, "agent": AGENT, "operator": "Arian", "lapse_s": lapse_s})
        shown = until(lambda: TITLE in posted(), 15, 0.3)
        return ask, nonce, t0, (round(time.time() - t0, 1) if shown else None)
    def end(ask): stub.send({"type": "join_end", "ask": ask})
    def session_call(token, tool):
        body = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": {}}}
        r = urllib.request.Request(f"http://127.0.0.1:{API_PORT}/mcp", method="POST", data=json.dumps(body).encode(),
                                   headers={"Authorization": f"Bearer {API_TOKEN}", "X-Desk-Session": token, "Content-Type": "application/json"})
        with urllib.request.urlopen(r, timeout=40) as a: return json.load(a)["result"]
    def restart_bridge(password=False):
        state["bridge"].terminate(); state["bridge"].wait(timeout=15); time.sleep(3); state["bridge"] = start_bridge(password)
        if not until(health, 30, 1): raise RuntimeError("the restarted bridge's API never answered")
    boot0 = sh("cat /proc/sys/kernel/random/boot_id", check=False).strip()
    def evidence(name):
        """What the device says right after a round failed: whether it rebooted, and its logs, this boot's and the last one's."""
        subprocess.run(["adb", "wait-for-device"], timeout=180, check=False)
        boot = sh("cat /proc/sys/kernel/random/boot_id", check=False).strip()
        log("evidence:", name, "boot id", boot, "was", boot0, "uptime", sh("cat /proc/uptime", check=False).strip())
        for what, cmd in (("logcat", "logcat -d -b all -v time"), ("logcat-last-boot", "logcat -L -b all -v time"), ("dmesg", "dmesg"),
                          ("tombstones", "ls -l /data/tombstones; for f in /data/tombstones/tombstone_*; do head -60 $f; done"),
                          ("dropbox", "dumpsys dropbox --print system_server_crash system_server_watchdog system_app_native_crash data_app_native_crash data_app_crash SYSTEM_TOMBSTONE 2>&1 | tail -400")):
            try: (out / f"{name}-{what}.txt").write_text(sh(cmd, check=False, timeout=60))
            except Exception as e: log("evidence:", what, e)
    def step(name, fn):
        try: fn()
        except (Exception, SystemExit) as e:
            record(name, False, f"the round itself failed: {type(e).__name__}: {str(e)[:400]}")
            try: evidence(name)
            except Exception as e2: log("evidence failed:", e2)
        finally:
            try: collapse(); restart_bridge(); sh("input keyevent KEYCODE_HOME", check=False)
            except Exception as e: log("cleanup:", e)

    def r1_accept():
        off = fresh_phone(); shot("r1-0-service-off")
        record("R1a-starts-with-the-service-off", not off["service_running"] and not off["listening"], f"before the request: {off}")
        ask, nonce, t0, shown = request()
        on_screen = notification_on_screen("r1-1-notification")
        record("R1b-notification-with-the-service-off", shown is not None and on_screen and not service_running(),
               f"posted after {shown}s; Accept and Deny on screen {on_screen}; service running {service_running()}")
        if not tap_label("Accept", seconds=10): raise RuntimeError("no Accept to tap")
        asked = tap_label("Screen only", seconds=15, name="r1-2-input-asked-first-time")
        start_now = tap_label("Start now", "Start", seconds=25, name="r1-3-start-now")
        record("R1c-first-time-input-question-then-start-now", asked is not None and start_now is not None, f"input question tapped {asked!r}; Start now tapped {start_now!r}")
        ready = until(lambda: stub.said(ask, t0), 40, 0.5)
        record("R1d-phone-says-ready", bool(ready) and ready.get("said") == "ready" and ready.get("nonce") == nonce,
               f"after Start now the phone said {ready}; service running {service_running()}; listening {listening()}")
        if not ready or ready.get("said") != "ready": return
        t1 = time.time()
        api("POST", "/offer", {"agent": AGENT, "ask": ask, "nonce": nonce, "accepted": True})
        answer = events.answer(ask, 30); second = login_prompts()
        token = api("POST", "/join", {"agent": AGENT, "ask": ask, "how": "person"})["session"] if answer == "allow" else None
        inside = None
        if token:
            try: got = session_call(token, "screenshot"); inside = not got.get("isError") and any(c.get("type") == "image" for c in got.get("content", []))
            except Exception as e: inside = f"{type(e).__name__}: {e}"
        time.sleep(2); shot("r1-4-agent-connected")
        record("R1e-agent-in-with-no-second-allow", answer == "allow" and inside is True and not second,
               f"bridge answered {answer!r} {round(time.time() - t1, 1)}s after the offer; RigDesk's login prompts shown {second}; agent's screenshot through the session: {inside}")
        want = f"{NAMED} joined: your Accept on this phone"; title, how = f"{NAMED} joined", "your Accept on this phone"; notice = f"{title}|By {how}."
        told = until(lambda: any(n.startswith(notice) for n in joined_notices()), 10, 0.5); drawn = notice_drawn(shade_shot("r1-5-joined-notice", f"By {how}."), title, how)
        record("R1g-the-phone-says-who-joined-and-how", told and drawn and not autogrouped(), f"joined notices {joined_notices()}; wanted {notice!r}; drawn in the shade {drawn}; autogrouped {autogrouped()}")
        end(ask); gone = until(lambda: TITLE not in posted(), 10, 0.3)
        record("R1f-join-end-leaves-nothing-posted", gone, f"after join_end: posted {posted()}")
        # The same knock is not let in twice: the ask's Accept went with its request.
        try: api("POST", "/offer/end", {"ask": ask})
        except Exception: pass
    def r2_forged():
        """The bridge claims an Accept the phone never took: the phone asks the person as before, it does not let the agent in."""
        on = {"service_running": service_running(), "listening": listening()}
        if not on["listening"]: raise RuntimeError(f"needs the service on from R1: {on}")
        n = next(counter); ask, nonce = f"desk_forged{n:03d}", f"nonce-{n:03d}-{os.urandom(8).hex()}"
        api("POST", "/offer", {"agent": AGENT, "ask": ask, "nonce": nonce, "accepted": True})
        prompted = until(login_prompts, 20, 0.5); answer = events.answer(ask, 3)
        sh("cmd statusbar expand-notifications", check=False); time.sleep(2); shot("r2-unaccepted-ask-asks-the-person")
        record("R2-an-accepted-claim-for-an-ask-the-phone-never-accepted-still-asks", bool(prompted) and answer is None,
               f"RigDesk's own prompt {prompted}; bridge answer {answer!r}")
        api("POST", "/offer/end", {"ask": ask})
    def no_stale_prompt(xml):
        labels = {x.strip().lower() for n in nodes(xml) for x in n[:2]}
        return "do you accept?" not in labels
    def r7_password():
        """Rig's Accept with the saved password while the phone's own prompt shows: the phone says the agent joined on the operator's
        Rig Accept with the saved password, and no "Do you accept?" is left once the prompt is taken away."""
        if not listening(): raise RuntimeError("needs the service on from R1")
        restart_bridge(password=True)
        n = next(counter); ask, nonce = f"desk_pw{n:03d}", f"nonce-{n:03d}-{os.urandom(8).hex()}"
        api("POST", "/offer", {"agent": AGENT, "ask": ask, "nonce": nonce})
        prompted = until(login_prompts, 20, 0.5)
        logged = api("POST", "/login", {"agent": AGENT, "ask": ask})
        want = f"{NAMED} joined: your Rig Accept, saved password"; title, how = f"{NAMED} joined", "your Rig Accept, saved password"; notice = f"{title}|By {how}."
        told = until(lambda: any(n.startswith(notice) for n in joined_notices()), 15, 0.5)
        drawn = notice_drawn(shade_shot("r7-1-joined-notice", f"By {how}."), title, how)
        api("POST", "/offer/end", {"ask": ask}); time.sleep(3)
        xml = app_shot("r7-2-app-after-the-prompt-went")
        card = any(want in (x[0] + x[1]) for x in nodes(xml))
        record("R7-password-route-says-rig-accept-saved-password", bool(prompted) and logged.get("logged_in") is True and told and drawn and no_stale_prompt(xml) and card and not autogrouped(),
               f"prompt shown {prompted}; /login {logged}; joined notices {joined_notices()}; wanted {notice!r}; drawn in the shade {drawn}; stale 'Do you accept?' left {not no_stale_prompt(xml)}; card says it {card}; autogrouped {autogrouped()}")
        sh("input keyevent KEYCODE_HOME", check=False)
    def r8_allow():
        """The person's Allow on the phone's own prompt: the phone says the agent joined on their Allow."""
        if not listening(): raise RuntimeError("needs the service on from R1")
        n = next(counter); ask, nonce = f"desk_allow{n:03d}", f"nonce-{n:03d}-{os.urandom(8).hex()}"
        api("POST", "/offer", {"agent": AGENT, "ask": ask, "nonce": nonce})
        prompted = until(login_prompts, 20, 0.5)
        share_screen_page()
        allowed = tap_label("Accept", seconds=20, name="r8-1-knock-prompt")
        answer = events.answer(ask, 20)
        token = api("POST", "/join", {"agent": AGENT, "ask": ask, "how": "person"})["session"] if answer == "allow" else None
        want = f"{NAMED} joined: your Allow on this phone"; title, how = f"{NAMED} joined", "your Allow on this phone"; notice = f"{title}|By {how}."
        told = until(lambda: any(n.startswith(notice) for n in joined_notices()), 15, 0.5)
        drawn = notice_drawn(shade_shot("r8-2-joined-notice", f"By {how}."), title, how); xml = app_shot("r8-3-app-card")
        card = any(want in (x[0] + x[1]) for x in nodes(xml))
        record("R8-allow-route-says-your-allow", bool(prompted) and allowed is not None and answer == "allow" and bool(token) and told and drawn and no_stale_prompt(xml) and card and not autogrouped(),
               f"prompt {prompted}; tapped {allowed!r}; bridge answered {answer!r}; joined notices {joined_notices()}; wanted {notice!r}; drawn in the shade {drawn}; stale prompt left {not no_stale_prompt(xml)}; card says it {card}; autogrouped {autogrouped()}")
        sh("input keyevent KEYCODE_HOME", check=False)
    def r3_deny():
        fresh_phone(); ask, nonce, t0, shown = request()
        on_screen = notification_on_screen("r3-1-notification")
        if not tap_label("Deny", seconds=10): raise RuntimeError("no Deny to tap")
        said = until(lambda: stub.said(ask, t0), 15, 0.3); gone = until(lambda: TITLE not in posted(), 10, 0.3)
        time.sleep(1); shot("r3-2-after-deny")
        record("R3-deny-says-no-and-leaves-nothing", on_screen and bool(said) and said.get("said") == "deny" and said.get("nonce") == nonce and gone and not service_running(),
               f"on screen {on_screen}; phone said {said}; notification gone {gone}; service running {service_running()}")
    def r4_withdrawn():
        fresh_phone(); ask, nonce, t0, shown = request()
        on_screen = notification_on_screen("r4-1-notification")
        t1 = time.time(); end(ask); gone = until(lambda: TITLE not in posted(), 10, 0.3); at = round(time.time() - t1, 1)
        time.sleep(1); shot("r4-2-after-withdrawal")
        record("R4-withdrawal-takes-the-notification-away", on_screen and gone and stub.said(ask, t0) is None, f"on screen {on_screen}; gone {gone} after {at}s; phone said {stub.said(ask, t0)}")
    def r5_lapse():
        fresh_phone(); ask, nonce, t0, shown = request(lapse_s=20)
        on_screen = notification_on_screen("r5-1-notification")
        collapse(); gone = until(lambda: TITLE not in posted(), 40, 0.5); at = round(time.time() - t0, 1)
        sh("cmd statusbar expand-notifications", check=False); time.sleep(2); shot("r5-2-after-lapse")
        record("R5-lapse-takes-the-notification-away-on-the-phones-clock", on_screen and gone and at >= 19, f"on screen {on_screen}; gone {gone} at {at}s of a 20 s lapse; phone said {stub.said(ask, t0)}")
    def r6_cancel_start_now():
        fresh_phone(); ask, nonce, t0, shown = request()
        notification_on_screen("r6-1-notification")
        if not tap_label("Accept", seconds=10): raise RuntimeError("no Accept to tap")
        asked_again = tap_label("Screen only", "Turn on", seconds=6) is not None
        cancel = tap_label("Cancel", seconds=25, name="r6-2-start-now")
        said = until(lambda: stub.said(ask, t0), 15, 0.3); time.sleep(1); shot("r6-3-after-cancel")
        record("R6-input-asked-once-and-cancelling-start-now-says-no", not asked_again and cancel is not None and bool(said) and said.get("said") == "deny" and not service_running(),
               f"input question again {asked_again}; Cancel tapped {cancel!r}; phone said {said}; service running {service_running()}")

    def rig_accept(ask, nonce):
        """What the rigdesk plugin sends when the operator accepted in Rig and the saved password found no login."""
        stub.send({"type": "join_request", "ask": ask, "nonce": nonce, "agent": AGENT, "operator": "Arian", "lapse_s": 180, "rig_accepted": True})
    def r9_rig_accept():
        """From cold, the operator's Accept in Rig: Android's Start now comes up with nothing pressed on the phone first, and the agent is in."""
        overlay(True)
        off = fresh_phone(); shot("r9-0-service-off")
        ask, nonce, t0, shown = request()
        notification_on_screen("r9-1-notification"); collapse()
        t1 = time.time(); rig_accept(ask, nonce)
        start_now = tap_label("Start now", "Start", seconds=20, name="r9-2-start-now-raised")
        raised_after = round(time.time() - t1, 1)
        record("R9a-rig-accept-raises-start-now-from-cold", not off["service_running"] and not off["listening"] and start_now is not None and not rig_accepted_posted(),
               f"before: {off}; Start now tapped {start_now!r} {raised_after}s after Rig's Accept, with no Accept pressed on the phone; fallback notification {rig_accepted_posted()}")
        ready = until(lambda: stub.said(ask, t0), 40, 0.5)
        gone = until(lambda: TITLE not in posted(), 10, 0.3)
        if not ready or ready.get("said") != "ready":
            record("R9b-agent-in-on-start-now", False, f"after Start now the phone said {ready}"); return
        api("POST", "/offer", {"agent": AGENT, "ask": ask, "nonce": nonce, "accepted": True})
        answer = events.answer(ask, 30); second = login_prompts()
        token = api("POST", "/join", {"agent": AGENT, "ask": ask, "how": "person"})["session"] if answer == "allow" else None
        inside = None
        if token:
            try: got = session_call(token, "screenshot"); inside = not got.get("isError") and any(c.get("type") == "image" for c in got.get("content", []))
            except Exception as e: inside = f"{type(e).__name__}: {e}"
        record("R9b-agent-in-on-start-now", answer == "allow" and inside is True and not second and gone,
               f"phone said {ready.get('said')}; bridge answered {answer!r}; second prompts {second}; screenshot through the session {inside}; notification gone {gone}")
        want = f"{NAMED} joined: your Rig Accept, then your Start now"; title, how = f"{NAMED} joined", "your Rig Accept, then your Start now"; notice = f"{title}|By {how}."
        told = until(lambda: any(n.startswith(notice) for n in joined_notices()), 15, 0.5)
        drawn = notice_drawn(shade_shot("r9-3-joined-notice", f"By {how}."), title, how); xml = app_shot("r9-4-app-card")
        card = any(want in (x[0] + x[1]) for x in nodes(xml))
        record("R9c-the-phone-says-rig-accept-then-start-now", told and drawn and card and not autogrouped(),
               f"joined notices {joined_notices()}; wanted {notice!r}; drawn in the shade {drawn}; card says it {card}; autogrouped {autogrouped()}")
        end(ask)
        try: api("POST", "/offer/end", {"ask": ask})
        except Exception: pass
    def r10_unraised():
        """Without the overlay permission Android keeps the screen from starting in the background: the phone posts the notification instead."""
        overlay(False)
        try:
            fresh_phone(); ask, nonce, t0, shown = request(); collapse()
            rig_accept(ask, nonce)
            raised = tap_label("Start now", "Start", seconds=6) is not None
            fallback = until(rig_accepted_posted, 15, 0.5)
            on_screen = notification_on_screen("r10-1-fallback-notification")
            accepted = tap_label("Accept", seconds=10) is not None
            start_now = tap_label("Start now", "Start", seconds=25, name="r10-2-start-now")
            ready = until(lambda: stub.said(ask, t0), 40, 0.5)
            record("R10-unraised-start-now-falls-back-to-the-notification", not raised and bool(fallback) and accepted and start_now is not None and bool(ready) and ready.get("said") == "ready",
                   f"raised with no overlay {raised}; fallback {fallback}; on screen {on_screen}; Accept {accepted}; Start now {start_now!r}; phone said {ready}")
            end(ask)
        finally:
            overlay(True)

    step("R1-accept", r1_accept)
    step("R2-forged", r2_forged)
    step("R7-password", r7_password)
    step("R8-allow", r8_allow)
    step("R3-deny", r3_deny)
    step("R4-withdrawn", r4_withdrawn)
    step("R5-lapse", r5_lapse)
    step("R6-cancel", r6_cancel_start_now)
    step("R9-rig-accept", r9_rig_accept)
    step("R10-unraised", r10_unraised)
    (out / "stub-frames.json").write_text(json.dumps([{"t": round(t, 2), **{k: v for k, v in f.items() if k != "tools"}} for t, f in stub.frames], indent=1))
    failed = [r["round"] for r in results if not r["ok"]]
    log("RESULT", f"{len(results) - len(failed)}/{len(results)} passed", failed)
    sys.exit(1 if failed else 0)

main()
