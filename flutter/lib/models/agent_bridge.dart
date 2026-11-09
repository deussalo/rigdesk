import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:http/http.dart' as http;

/// Where the agent host for one PC is: the rigdesk-bridge on the Rig host that keeps the agent's own session to the
/// PC. The viewer shows the PC itself; this carries the agent panel's buttons and the agent's state.
class AgentHost {
  final String url;
  final String token;

  /// The PC's name in Rig's plugin settings, which a share link carries.
  final String name;

  const AgentHost({required this.url, required this.token, required this.name});

  static AgentHost? fromJson(String text) {
    try {
      final j = jsonDecode(text) as Map<String, dynamic>;
      final host = AgentHost(
          url: ((j['url'] ?? '') as String).trim().replaceAll(RegExp(r'/+$'), ''),
          token: ((j['token'] ?? '') as String).trim(),
          name: ((j['name'] ?? '') as String).trim());
      return host.isComplete ? host : null;
    } catch (_) {
      return null;
    }
  }

  String toJson() => jsonEncode({'url': url, 'token': token, 'name': name});

  bool get isComplete =>
      (url.startsWith('http://') || url.startsWith('https://')) &&
      token.isNotEmpty &&
      RegExp(r'^[A-Za-z0-9.-]{1,253}$').hasMatch(name);
}

/// The agent panel's link to one agent host: its /viewer routes for the human's buttons, and its /events stream
/// turned into the `agent_state` JSON the panel already reads (holder, owner, link, actions, frame).
class AgentBridge {
  final AgentHost host;
  final void Function(String stateJson) onState;
  final void Function(String why) onTrouble;
  final http.Client _client;

  String _holder = '';
  String _ownerTitle = '';
  String _ownerGraph = '';
  bool _linkOpen = false;
  int _lastEvent = 0;
  String _boot = '';
  bool _closed = false;
  bool _down = false;

  AgentBridge(this.host,
      {required this.onState, required this.onTrouble, http.Client? client})
      : _client = client ?? http.Client();

  Map<String, String> get _auth => {'Authorization': 'Bearer ${host.token}'};

  void start() => unawaited(_follow());

  void close() {
    _closed = true;
    _client.close();
  }

  /// One of the panel's verbs: share, hand, take_back, stop, block, unblock, revoke, note (text and an optional
  /// PNG) or frame (a still of the PC's screen).
  Future<void> ask(String verb, {String text = '', Uint8List? png, int display = 0}) async {
    try {
      switch (verb) {
        case 'share':
          final minted = await _post('/viewer/mint', {});
          _linkOpen = true;
          _emit(link: 'rigdesk://${host.name}/${minted['grant']}#${minted['secret']}');
          break;
        case 'note':
          await _post('/viewer/note', {
            'text': text,
            'pngs': [if (png != null && png.isNotEmpty) base64Encode(png)],
          });
          break;
        case 'frame':
          final shot = await _client
              .get(Uri.parse('${host.url}/viewer/frame?display=$display'), headers: _auth)
              .timeout(const Duration(seconds: 20));
          if (shot.statusCode != 200) throw 'the agent host gave no frame: ${shot.body}';
          _emit(frame: shot.bodyBytes);
          break;
        default:
          final path = {
            'hand': 'hand',
            'take_back': 'human',
            'stop': 'stop',
            'block': 'block',
            'unblock': 'unblock',
            'revoke': 'revoke',
          }[verb];
          if (path == null) return;
          final said = await _post('/viewer/$path', {});
          _standing(said['standing']);
          _emit();
      }
    } catch (e) {
      onTrouble('RigDesk agent host: $e');
      if (verb == 'frame') _emit(note: 'No still of the screen: $e');
    }
  }

  Future<Map<String, dynamic>> _post(String path, Map<String, dynamic> body) async {
    final answer = await _client
        .post(Uri.parse('${host.url}$path'),
            headers: {..._auth, 'Content-Type': 'application/json'}, body: jsonEncode(body))
        .timeout(const Duration(seconds: 20));
    if (answer.statusCode != 200) throw '${answer.statusCode} ${answer.body}';
    final j = jsonDecode(answer.body);
    return j is Map<String, dynamic> ? j : {};
  }

  void _standing(dynamic standing) {
    if (standing is! Map) return;
    final owner = standing['owner'];
    _ownerTitle = owner is Map ? (owner['title'] ?? '') as String : '';
    _ownerGraph = owner is Map ? (owner['graph'] ?? '') as String : '';
    _holder = owner is Map ? (standing['holder'] ?? '') as String : '';
    _linkOpen = (standing['link_open'] ?? false) as bool;
  }

  void _emit({String? link, Map<String, dynamic>? action, String note = '', Uint8List? frame}) {
    if (_closed) return;
    final grant = link == null ? '' : link.split('/').last.split('#').first;
    final secret = link == null ? '' : link.split('#').last;
    onState(jsonEncode({
      'holder': _holder,
      'owner_title': _ownerTitle,
      'owner_graph': _ownerGraph,
      'link_open': _linkOpen,
      'host': host.name,
      'grant': grant,
      'secret': secret,
      'action': action,
      'note': note,
      'frame': frame == null ? '' : base64Encode(frame),
    }));
  }

  /// Applies one event from the agent host's /events stream.
  void apply(Map<String, dynamic> event) {
    switch (event['type']) {
      case 'claimed':
        final owner = event['owner'];
        _ownerTitle = owner is Map ? (owner['title'] ?? '') as String : '';
        _ownerGraph = owner is Map ? (owner['graph'] ?? '') as String : '';
        _holder = 'agent';
        _linkOpen = false;
        _emit();
        break;
      case 'holder':
        _holder = (event['holder'] ?? '') as String;
        _emit();
        break;
      case 'revoked':
        _holder = '';
        _ownerTitle = '';
        _ownerGraph = '';
        _emit();
        break;
      case 'action':
        _emit(action: {
          'agent_title': event['agent_title'],
          'tool': event['tool'],
          'summary': event['summary'],
          'ok': event['ok'],
          'x': event['x'],
          'y': event['y'],
        });
        break;
    }
  }

  Future<void> _follow() async {
    var wait = 1;
    while (!_closed) {
      try {
        final health = await _client
            .get(Uri.parse('${host.url}/health'), headers: _auth)
            .timeout(const Duration(seconds: 15));
        if (health.statusCode != 200) throw 'the agent host answered ${health.statusCode}';
        final j = jsonDecode(health.body) as Map<String, dynamic>;
        final boot = (j['boot'] ?? '') as String;
        if (boot != _boot) {
          _boot = boot;
          _lastEvent = 0;
        }
        _standing(j['standing']);
        _emit();
        final request = http.Request('GET', Uri.parse('${host.url}/events'))
          ..headers.addAll({..._auth, 'Accept': 'text/event-stream', 'Last-Event-ID': '$_lastEvent'});
        final stream = await _client.send(request);
        if (stream.statusCode != 200) throw 'the agent host answered ${stream.statusCode}';
        wait = 1;
        _down = false;
        final data = <String>[];
        await for (final line in stream.stream.transform(utf8.decoder).transform(const LineSplitter())) {
          if (_closed) return;
          if (line.startsWith('id:')) {
            final id = int.tryParse(line.substring(3).trim()) ?? 0;
            if (id > 0) _lastEvent = id;
          } else if (line.startsWith('data:')) {
            data.add(line.substring(5).trimLeft());
          } else if (line.isEmpty && data.isNotEmpty) {
            final event = jsonDecode(data.join('\n'));
            data.clear();
            if (event is Map<String, dynamic>) apply(event);
          }
        }
      } catch (e) {
        if (_closed) return;
        if (!_down) onTrouble('RigDesk agent host unreachable: $e');
        _down = true;
      }
      await Future.delayed(Duration(seconds: wait));
      wait = wait >= 30 ? 30 : wait * 2;
    }
  }
}
