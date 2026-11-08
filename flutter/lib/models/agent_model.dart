import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter/foundation.dart';

/// One tool call the agent made on the PC, as the action strip and cursor label show it.
class AgentAction {
  final String agentTitle;
  final String tool;
  final String summary;
  final bool ok;
  final int? x;
  final int? y;

  AgentAction.fromJson(Map<String, dynamic> j)
      : agentTitle = (j['agent_title'] ?? '') as String,
        tool = (j['tool'] ?? '') as String,
        summary = (j['summary'] ?? '') as String,
        ok = (j['ok'] ?? false) as bool,
        x = j['x'] as int?,
        y = j['y'] as int?;

  bool get hasPoint => x != null && y != null;
}

/// What one state from the PC asks of the UI beyond a redraw.
class AgentUpdate {
  final String? link;
  final String note;
  final Uint8List? frame;

  const AgentUpdate({this.link, this.note = '', this.frame});
}

/// The PC's share with a Rig agent, as this viewer last heard it. The PC owns the state; this only mirrors it.
class AgentModel with ChangeNotifier {
  static final Map<String, AgentModel> _bySession = {};

  static AgentModel of(Object sessionId) =>
      _bySession.putIfAbsent('$sessionId', () => AgentModel());

  static const maxActions = 6;

  /// '' (nobody holds the share), 'human', 'agent' or 'blocked'.
  String holder = '';
  String ownerTitle = '';
  String ownerGraph = '';
  bool linkOpen = false;
  String host = '';
  final List<AgentAction> actions = [];
  AgentAction? pointer;

  /// Set while the message box waits for a still of the screen.
  Completer<Uint8List?>? frameWaiter;

  bool get isShared => ownerTitle.isNotEmpty;
  bool get isBlocked => holder == 'blocked';
  bool get agentHolds => holder == 'agent';

  static String shareLink(String host, String grant, String secret) =>
      'rigdesk://$host/$grant#$secret';

  AgentUpdate apply(String stateJson) {
    final s = jsonDecode(stateJson) as Map<String, dynamic>;
    holder = (s['holder'] ?? '') as String;
    ownerTitle = (s['owner_title'] ?? '') as String;
    ownerGraph = (s['owner_graph'] ?? '') as String;
    linkOpen = (s['link_open'] ?? false) as bool;
    host = (s['host'] ?? '') as String;
    if (!isShared) {
      actions.clear();
      pointer = null;
    }
    final a = s['action'];
    if (a is Map<String, dynamic>) {
      final action = AgentAction.fromJson(a);
      actions.add(action);
      if (actions.length > maxActions) actions.removeAt(0);
      if (action.hasPoint) pointer = action;
    }
    final grant = (s['grant'] ?? '') as String;
    final secret = (s['secret'] ?? '') as String;
    final frame = (s['frame'] ?? '') as String;
    final update = AgentUpdate(
      link: grant.isNotEmpty && secret.isNotEmpty
          ? shareLink(host, grant, secret)
          : null,
      note: (s['note'] ?? '') as String,
      frame: frame.isEmpty ? null : base64Decode(frame),
    );
    final waiter = frameWaiter;
    if (waiter != null &&
        !waiter.isCompleted &&
        (update.frame != null || update.note.isNotEmpty)) {
      frameWaiter = null;
      waiter.complete(update.frame);
    }
    notifyListeners();
    return update;
  }
}
