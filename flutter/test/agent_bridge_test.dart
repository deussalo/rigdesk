import 'dart:convert';

import 'package:flutter_hbb/models/agent_bridge.dart';
import 'package:flutter_hbb/models/agent_model.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

const _host = AgentHost(url: 'http://127.0.0.1:21120', token: 't0k', name: 'deus');

void main() {
  test('plain http carries the token only to loopback and the tailnet, and an unreadable address is refused', () {
    for (final url in ['http://127.0.0.1:21120', 'http://localhost:1', 'http://100.64.0.1:1', 'http://100.127.255.255', 'http://pc.tailfae3b0.ts.net',
        'https://example.com', 'https://10.0.2.2:1']) {
      expect(plainHttpTo(url), isNull, reason: url);
    }
    for (final (url, to) in [('http://10.0.2.2:1', '10.0.2.2'), ('http://100.064.0.1:1', '100.064.0.1'), ('http://my_box.example', 'my_box.example'),
        ('http://100.128.0.1', '100.128.0.1'), ('http://127.0.0.2', '127.0.0.2'), ('HTTP://example.com', 'example.com'), ('http://', 'http://'), ('http://[', 'http://[')]) {
      expect(plainHttpTo(url), to, reason: url);
    }
  });

  test('a refused host gets no request, and the person is told why', () async {
    var requests = 0;
    final told = <String>[];
    final frames = <String>[];
    final bridge = AgentBridge(const AgentHost(url: 'http://my_box.example:1', token: 't0k', name: 'deus'),
        client: MockClient((request) async {
          requests++;
          return http.Response('{}', 200);
        }),
        onState: frames.add,
        onTrouble: told.add);
    bridge.start();
    await Future<void>.delayed(Duration.zero);
    for (final verb in ['share', 'stop', 'note', 'frame']) {
      await bridge.ask(verb);
    }
    bridge.close();
    expect(requests, 0);
    expect(told, hasLength(5));
    expect(told, everyElement(startsWith('Refused: plain http to my_box.example')));
  });

  test('share mints a link on the agent host naming the PC as Rig knows it',
      () async {
    final asked = <String>[];
    final client = MockClient((request) async {
      asked.add('${request.method} ${request.url.path} ${request.headers['Authorization']}');
      return http.Response(jsonEncode({'grant': 'aa' * 16, 'secret': 'bb' * 16}), 200);
    });
    final agent = AgentModel();
    String? link;
    final bridge = AgentBridge(_host,
        client: client,
        onState: (state) => link ??= agent.apply(state).link,
        onTrouble: (why) => fail(why));
    await bridge.ask('share');
    expect(asked, ['POST /viewer/mint Bearer t0k']);
    expect(link, 'rigdesk://deus/${'aa' * 16}#${'bb' * 16}');
    expect(agent.linkOpen, isTrue);
  });

  test('the agent host\'s events show its owner, holder and actions, and Stop takes input back',
      () async {
    final client = MockClient((request) async => http.Response(
        jsonEncode({'changed': true, 'standing': {'holder': 'human', 'owner': {'title': 'Front desk', 'graph': 'clients'}}}),
        200));
    final agent = AgentModel();
    final bridge = AgentBridge(_host,
        client: client, onState: agent.apply, onTrouble: (why) => fail(why));
    bridge.apply({'type': 'claimed', 'owner': {'agent_id': 'a1', 'title': 'Front desk', 'graph': 'clients'}});
    expect(agent.ownerTitle, 'Front desk');
    expect(agent.agentHolds, isTrue);
    bridge.apply({'type': 'action', 'agent_title': 'Front desk', 'tool': 'click', 'summary': 'click 10,20', 'ok': true, 'x': 10, 'y': 20});
    expect(agent.actions.single.summary, 'click 10,20');
    expect(agent.pointer?.x, 10);
    await bridge.ask('stop');
    expect(agent.holder, 'human');
    bridge.apply({'type': 'revoked', 'agent_id': 'a1', 'reason': 'the agent left'});
    expect(agent.isShared, isFalse);
  });
}
