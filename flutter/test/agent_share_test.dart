import 'dart:convert';
import 'dart:ui' as ui;

import 'package:flutter/painting.dart';
import 'package:flutter_hbb/models/agent_model.dart';
import 'package:flutter_hbb/models/agent_note.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('a minted share copies a rigdesk link naming the PC, grant and secret',
      () {
    final agent = AgentModel();
    final minted = agent.apply(jsonEncode({
      'holder': '',
      'owner_title': '',
      'link_open': true,
      'host': 'deus',
      'grant': '0123456789abcdef0123456789abcdef',
      'secret': 'fedcba9876543210fedcba9876543210',
    }));
    expect(minted.link,
        'rigdesk://deus/0123456789abcdef0123456789abcdef#fedcba9876543210fedcba9876543210');

    final claimed = agent.apply(jsonEncode({
      'holder': 'agent',
      'owner_title': 'Front desk',
      'owner_graph': 'clients',
      'host': 'deus',
      'action': {
        'agent_title': 'Front desk',
        'tool': 'click',
        'summary': 'click 10,20',
        'ok': true,
        'x': 10,
        'y': 20,
      },
    }));
    expect(claimed.link, isNull);
    expect(agent.ownerTitle, 'Front desk');
    expect(agent.agentHolds, isTrue);
    expect(agent.pointer?.summary, 'click 10,20');
  });

  testWidgets('a clip is sent cut to its region with the annotations drawn on',
      (tester) async {
    await tester.runAsync(() async {
      final recorder = ui.PictureRecorder();
      Canvas(recorder).drawRect(
          const Rect.fromLTWH(0, 0, 100, 80), Paint()..color = const Color(0xFFFFFFFF));
      final frame = await recorder.endRecording().toImage(100, 80);
      final png = await bakeNote(
        frame,
        const Rect.fromLTWH(20, 10, 40, 40),
        [
          NoteMark(NoteTool.box, [const Offset(25, 15), const Offset(45, 35)])
        ],
      );
      final codec = await ui.instantiateImageCodec(png);
      final baked = (await codec.getNextFrame()).image;
      expect(baked.width, 40);
      expect(baked.height, 40);
      final rgba = (await baked.toByteData(format: ui.ImageByteFormat.rawRgba))!;
      int red(int x, int y) => rgba.getUint8((y * 40 + x) * 4);
      int green(int x, int y) => rgba.getUint8((y * 40 + x) * 4 + 1);
      // The box's top edge runs through (15, 5) in the clip; its inside stays white.
      expect(red(15, 5), greaterThan(200));
      expect(green(15, 5), lessThan(120));
      expect(green(15, 15), greaterThan(200));
    });
  });
}
