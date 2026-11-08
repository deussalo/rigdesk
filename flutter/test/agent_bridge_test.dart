import 'dart:convert';

import 'package:flutter_hbb/models/agent_bridge.dart';
import 'package:flutter_hbb/models/agent_model.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

const _host = AgentHost(url: 'http://127.0.0.1:21120', token: 't0k', name: 'deus');

void main() {
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
